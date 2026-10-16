//! MCP's OAuth sign-in (PX-42) over HTTP: what a client such as Claude
//! does, from its registration to its tokens, with the user signing in to
//! the web UI with a passkey and answering the consent page.

use std::net::SocketAddr;

use jsonrpsee::ws_client::WsClient;
use reqwest::StatusCode;
use serde_json::{Value, json};
use spoolframe_admin_api::{AdminApiClient, MeApiClient, NewUser, Role};
use spoolframe_server::admin::{self, Admin, Origins, SetupCode, TrustedProxies};
use spoolframe_server::auth::Auth;
use spoolframe_server::oauth::McpSwitch;
use spoolframe_server::passkeys::Passkeys;
use spoolframe_server::store::Store;

use crate::admin::{CODE, anonymous, server_id, setup, state_dir, with_token};
use crate::passkeys::{Authenticator, ORIGIN, add_passkey, cookie, sign_in_with};

const PASSWORD: &str = "correct horse";
/// Where the client wants the code: Claude's callback.
pub(crate) const RETURN: &str = "https://claude.ai/api/mcp/auth_callback";
/// A PKCE verifier and its S256 challenge.
pub(crate) const VERIFIER: &str = "dBjftJeZ4CVP-mJ92K9b9sFvMDqpH9VZlIfqXMQ7fFZ";
pub(crate) const CHALLENGE: &str = "v5EpdpfSvdxcI3-JDrrt8W3BNL0gE6xL5Fxd211vTEY";
/// The client's own site, where a browser-based client calls from.
const CLIENT_ORIGIN: &str = "https://claude.ai";

/// An admin listener at [`ORIGIN`] with MCP switched `on`, and its switch.
pub(crate) async fn mcp_server(
    test: &str,
    on: bool,
) -> (SocketAddr, McpSwitch, spoolframe_test_dir::TestDir) {
    let dir = state_dir(test).await;
    let auth = Auth::new(Store::open(dir.as_ref()).await.unwrap());
    let origins = Origins::Exactly(ORIGIN.to_owned());
    let mcp = McpSwitch::new(on);
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            passkeys: std::sync::Arc::new(Passkeys::new(&origins, &server_id())),
            mcp: mcp.clone(),
            ..crate::admin::admin(auth, Some(SetupCode::new(CODE)))
        },
        origins,
        TrustedProxies::default(),
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    (addr, mcp, dir)
}

/// A browser, or an HTTP client, that does not follow redirects: where
/// each one goes is what is checked.
pub(crate) fn browser() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// `root`, set up; and `alice`, a consumer, signed in on the socket.
pub(crate) async fn users(addr: SocketAddr) -> (WsClient, WsClient) {
    let root = anonymous(addr)
        .await
        .setup(setup(CODE, "root", PASSWORD))
        .await
        .unwrap();
    let root = with_token(addr, &root.token).await;
    root.user_add(NewUser {
        username: "alice".into(),
        display_name: Some("Alice".into()),
        password: PASSWORD.into(),
        role: Role::Consumer,
    })
    .await
    .unwrap();
    let alice = anonymous(addr)
        .await
        .login(crate::admin::creds("alice", PASSWORD))
        .await
        .unwrap();
    (root, with_token(addr, &alice.token).await)
}

/// `user` adds a passkey on a phone, and signs a browser in with it: the
/// browser's session cookie.
pub(crate) async fn passkey_cookie(addr: SocketAddr, user: &WsClient) -> String {
    let mut phone = Authenticator::new();
    add_passkey(user, &mut phone, "iPhone").await;
    let signed_in = sign_in_with(addr, &mut phone).await;
    assert_eq!(signed_in.status(), StatusCode::OK);
    cookie(&signed_in)
}

/// The client registers itself, from its own site: its `client_id`.
pub(crate) async fn register(addr: SocketAddr, name: &str) -> String {
    let response = browser()
        .post(format!("http://{addr}/register"))
        .header("origin", CLIENT_ORIGIN)
        .json(&json!({ "client_name": name, "redirect_uris": [RETURN] }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["token_endpoint_auth_method"], "none");
    body["client_id"].as_str().unwrap().to_owned()
}

/// `/authorize`'s query for `client_id`, asking for `scope`.
pub(crate) fn authorize_query(client_id: &str, scope: &str) -> Vec<(&'static str, String)> {
    vec![
        ("response_type", "code".into()),
        ("client_id", client_id.into()),
        ("redirect_uri", RETURN.into()),
        ("scope", scope.into()),
        ("state", "s-123".into()),
        ("code_challenge", CHALLENGE.into()),
        ("code_challenge_method", "S256".into()),
        ("resource", format!("{ORIGIN}/mcp")),
    ]
}

async fn authorize(addr: SocketAddr, query: &[(&str, String)]) -> reqwest::Response {
    browser()
        .get(format!("http://{addr}/authorize"))
        .query(query)
        .send()
        .await
        .unwrap()
}

/// The request id `/authorize` sent the browser to the consent page with.
pub(crate) async fn begin(addr: SocketAddr, client_id: &str, scope: &str) -> String {
    let response = authorize(addr, &authorize_query(client_id, scope)).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let to = response.headers()["location"].to_str().unwrap();
    to.strip_prefix("/connect?request=")
        .unwrap_or_else(|| panic!("{to}"))
        .to_owned()
}

pub(crate) async fn pending(addr: SocketAddr, cookie: Option<&str>, id: &str) -> reqwest::Response {
    let mut request = browser().get(format!("http://{addr}/oauth/pending/{id}"));
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    request.send().await.unwrap()
}

/// The consent page's answer, from the web UI's own page.
pub(crate) async fn decide(
    addr: SocketAddr,
    cookie: &str,
    id: &str,
    allow: bool,
    scopes: &[&str],
) -> reqwest::Response {
    decide_from(addr, ORIGIN, cookie, id, allow, scopes).await
}

async fn decide_from(
    addr: SocketAddr,
    origin: &str,
    cookie: &str,
    id: &str,
    allow: bool,
    scopes: &[&str],
) -> reqwest::Response {
    browser()
        .post(format!("http://{addr}/oauth/decide"))
        .header("origin", origin)
        .header("cookie", cookie)
        .json(&json!({ "request": id, "allow": allow, "scopes": scopes }))
        .send()
        .await
        .unwrap()
}

/// `name`'s value in the URL `to`'s query.
pub(crate) fn param(to: &str, name: &str) -> Option<String> {
    url::Url::parse(to)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

pub(crate) async fn token(addr: SocketAddr, form: &[(&str, &str)]) -> reqwest::Response {
    browser()
        .post(format!("http://{addr}/token"))
        .header("origin", CLIENT_ORIGIN)
        .form(form)
        .send()
        .await
        .unwrap()
}

/// The whole flow for the user signed in by `cookie`, granting `scopes`
/// of what the client asks for (`asked`): the token response.
pub(crate) async fn connect(
    addr: SocketAddr,
    cookie: &str,
    client_id: &str,
    asked: &str,
    scopes: &[&str],
) -> Value {
    let id = begin(addr, client_id, asked).await;
    let decided = decide(addr, cookie, &id, true, scopes).await;
    assert_eq!(decided.status(), StatusCode::OK);
    let to: Value = decided.json().await.unwrap();
    let code = param(to["redirect"].as_str().unwrap(), "code").unwrap();
    let response = token(
        addr,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", &code),
            ("redirect_uri", RETURN),
            ("code_verifier", VERIFIER),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

#[tokio::test]
async fn the_metadata_documents_name_this_server_and_its_endpoints() {
    let (addr, _mcp, _dir) = mcp_server("oauth-metadata", true).await;
    let server: Value = reqwest::get(format!(
        "http://{addr}/.well-known/oauth-authorization-server"
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(server["issuer"], ORIGIN);
    assert_eq!(
        server["authorization_endpoint"],
        format!("{ORIGIN}/authorize")
    );
    assert_eq!(server["token_endpoint"], format!("{ORIGIN}/token"));
    assert_eq!(
        server["registration_endpoint"],
        format!("{ORIGIN}/register")
    );
    assert_eq!(server["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(
        server["scopes_supported"],
        json!(["library:read", "playlists:read", "playlists:write", "admin"])
    );
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let resource: Value = reqwest::get(format!("http://{addr}{path}"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resource["resource"], format!("{ORIGIN}/mcp"), "{path}");
        assert_eq!(resource["authorization_servers"], json!([ORIGIN]), "{path}");
    }
}

/// Without `admin.public_url`, a request to a name is refused: the name
/// could be anyone's DNS. An address names this listener.
#[tokio::test]
async fn without_a_public_url_the_issuer_is_the_listener_s_own_address() {
    let dir = state_dir("oauth-issuer").await;
    let auth = Auth::new(Store::open(dir.as_ref()).await.unwrap());
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            mcp: McpSwitch::new(true),
            ..crate::admin::admin(auth, None)
        },
        Origins::default(),
        TrustedProxies::default(),
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    let url = format!("http://{addr}/.well-known/oauth-authorization-server");
    let server: Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
    assert_eq!(server["issuer"], format!("http://{addr}"));
    let by_name = browser()
        .get(&url)
        .header("host", "rebind.example")
        .send()
        .await
        .unwrap();
    assert_eq!(by_name.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn while_mcp_is_off_none_of_it_is_found() {
    let (addr, mcp, _dir) = mcp_server("oauth-off", false).await;
    let (_root, alice) = users(addr).await;
    let get = |path: &str| browser().get(format!("http://{addr}{path}")).send();
    for path in [
        "/.well-known/oauth-authorization-server",
        "/.well-known/oauth-protected-resource/mcp",
        "/authorize?client_id=x",
        "/oauth/pending/x",
    ] {
        assert_eq!(
            get(path).await.unwrap().status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    for path in ["/register", "/token"] {
        let response = browser()
            .post(format!("http://{addr}{path}"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    // Turned on, at once; and off again, at once.
    mcp.set(true);
    let client = register(addr, "Claude").await;
    let cookie = passkey_cookie(addr, &alice).await;
    let id = begin(addr, &client, "library:read").await;
    mcp.set(false);
    assert_eq!(
        decide(addr, &cookie, &id, true, &["library:read"])
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

/// The ticket's first check, as far as a server alone can make it: a
/// client registers by the URL alone, its user signs in with a passkey,
/// consents to some of what it asked, and the client gets tokens for just
/// that. `playback` (before PX-05) and `admin` (alice is no admin) are
/// never offered.
#[tokio::test]
async fn a_client_registers_its_user_signs_in_with_a_passkey_and_consents() {
    let (addr, _mcp, _dir) = mcp_server("oauth-flow", true).await;
    let (_root, alice) = users(addr).await;
    let cookie = passkey_cookie(addr, &alice).await;
    let client = register(addr, "Claude").await;

    let id = begin(addr, &client, "library:read playlists:write playback admin").await;
    // The consent page asks as the user signed in, and no one else.
    assert_eq!(
        pending(addr, None, &id).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let shown: Value = pending(addr, Some(&cookie), &id)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        shown,
        json!({
            "clientName": "Claude",
            "returnHost": "claude.ai",
            "scopes": ["library:read", "playlists:write"],
        })
    );

    let decided = decide(addr, &cookie, &id, true, &["library:read"]).await;
    assert_eq!(decided.status(), StatusCode::OK);
    let to: Value = decided.json().await.unwrap();
    let to = to["redirect"].as_str().unwrap();
    assert!(to.starts_with(&format!("{RETURN}?")), "{to}");
    assert_eq!(param(to, "state").as_deref(), Some("s-123"));
    let code = param(to, "code").unwrap();

    let response = token(
        addr,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", &client),
            ("code", &code),
            ("redirect_uri", RETURN),
            ("code_verifier", VERIFIER),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let tokens: Value = response.json().await.unwrap();
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["expires_in"], 3600);
    assert_eq!(tokens["scope"], "library:read");

    let refreshed = token(
        addr,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &client),
            ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
        ],
    )
    .await;
    assert_eq!(refreshed.status(), StatusCode::OK);
    let again: Value = refreshed.json().await.unwrap();
    assert_ne!(again["access_token"], tokens["access_token"]);
}

/// An admin is offered `admin`, and may grant it alone.
#[tokio::test]
async fn an_admin_is_offered_admin() {
    let (addr, _mcp, _dir) = mcp_server("oauth-admin", true).await;
    let (root, _alice) = users(addr).await;
    let cookie = passkey_cookie(addr, &root).await;
    let client = register(addr, "Claude").await;
    let id = begin(addr, &client, "library:read admin").await;
    let shown: Value = pending(addr, Some(&cookie), &id)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(shown["scopes"], json!(["library:read", "admin"]));
    let tokens = connect(addr, &cookie, &client, "library:read admin", &["admin"]).await;
    assert_eq!(tokens["scope"], "admin");
}

/// A request with a return address its registration does not have, or
/// without PKCE, errors on the server's page and redirects nowhere.
#[tokio::test]
async fn a_broken_request_errors_on_the_server_s_page_and_redirects_nowhere() {
    let (addr, _mcp, _dir) = mcp_server("oauth-broken", true).await;
    let client = register(addr, "Claude").await;
    let good = authorize_query(&client, "library:read");
    let changed = |name: &str, value: Option<&str>| -> Vec<(&'static str, String)> {
        good.iter()
            .filter(|(k, _)| *k != name)
            .cloned()
            .chain(value.map(|v| {
                let key = good.iter().find(|(k, _)| *k == name).unwrap().0;
                (key, v.to_owned())
            }))
            .collect()
    };
    let cases = [
        (
            "an unregistered return address",
            changed("redirect_uri", Some("https://evil.example/cb")),
        ),
        ("no PKCE", changed("code_challenge", None)),
        (
            "plain PKCE",
            changed("code_challenge_method", Some("plain")),
        ),
        ("an unknown client", changed("client_id", Some("nope"))),
        (
            "another server's resource",
            changed("resource", Some("https://other.example/mcp")),
        ),
    ];
    for (what, query) in cases {
        let response = authorize(addr, &query).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{what}");
        assert!(response.headers().get("location").is_none(), "{what}");
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html"),
            "{what}"
        );
        let page = response.text().await.unwrap();
        assert!(!page.contains("evil.example"), "{what}: {page}");
    }
}

#[tokio::test]
async fn denying_sends_the_client_access_denied() {
    let (addr, _mcp, _dir) = mcp_server("oauth-deny", true).await;
    let (_root, alice) = users(addr).await;
    let cookie = passkey_cookie(addr, &alice).await;
    let client = register(addr, "Claude").await;
    let id = begin(addr, &client, "library:read").await;
    let to: Value = decide(addr, &cookie, &id, false, &[])
        .await
        .json()
        .await
        .unwrap();
    let to = to["redirect"].as_str().unwrap();
    assert_eq!(param(to, "error").as_deref(), Some("access_denied"));
    assert_eq!(param(to, "code"), None);
}

/// Consent is the web UI's own page's: another site's page cannot answer
/// it for a signed-in browser.
#[tokio::test]
async fn consent_from_another_site_is_refused() {
    let (addr, _mcp, _dir) = mcp_server("oauth-csrf", true).await;
    let (_root, alice) = users(addr).await;
    let cookie = passkey_cookie(addr, &alice).await;
    let client = register(addr, "Claude").await;
    let id = begin(addr, &client, "library:read").await;
    let forged = decide_from(addr, CLIENT_ORIGIN, &cookie, &id, true, &["library:read"]).await;
    assert_eq!(forged.status(), StatusCode::FORBIDDEN);
}

/// The client's endpoints are open to its own site: a preflight is
/// answered, and its calls are not refused for their origin.
#[tokio::test]
async fn the_client_s_endpoints_take_calls_from_its_site() {
    let (addr, _mcp, _dir) = mcp_server("oauth-cors", true).await;
    let preflight = browser()
        .request(reqwest::Method::OPTIONS, format!("http://{addr}/token"))
        .header("origin", CLIENT_ORIGIN)
        .header("access-control-request-method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(preflight.headers()["access-control-allow-origin"], "*");
    assert!(
        preflight
            .headers()
            .get("access-control-allow-credentials")
            .is_none()
    );

    let refused = token(addr, &[("grant_type", "authorization_code")]).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(refused.headers()["access-control-allow-origin"], "*");
    let body: Value = refused.json().await.unwrap();
    assert_eq!(body["error"], "invalid_request");
}

#[tokio::test]
async fn the_token_endpoint_names_what_is_wrong() {
    let (addr, _mcp, _dir) = mcp_server("oauth-token", true).await;
    let (_root, alice) = users(addr).await;
    let cookie = passkey_cookie(addr, &alice).await;
    let client = register(addr, "Claude").await;
    let unknown = token(
        addr,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", "nope"),
            ("refresh_token", "sfo_x"),
        ],
    )
    .await;
    assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        unknown.json::<Value>().await.unwrap()["error"],
        "invalid_client"
    );

    let id = begin(addr, &client, "library:read").await;
    let to: Value = decide(addr, &cookie, &id, true, &["library:read"])
        .await
        .json()
        .await
        .unwrap();
    let code = param(to["redirect"].as_str().unwrap(), "code").unwrap();
    let wrong = token(
        addr,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", &client),
            ("code", &code),
            ("redirect_uri", RETURN),
            (
                "code_verifier",
                "dBjftJeZ4CVP-mJ92K9b9sFvMDqpH9VZlIfqXMQ7fFz",
            ),
        ],
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        wrong.json::<Value>().await.unwrap()["error"],
        "invalid_grant"
    );

    let other = token(addr, &[("grant_type", "password"), ("client_id", &client)]).await;
    assert_eq!(
        other.json::<Value>().await.unwrap()["error"],
        "unsupported_grant_type"
    );
}

#[tokio::test]
async fn registrations_are_held_to_https_or_loopback() {
    let (addr, _mcp, _dir) = mcp_server("oauth-register", true).await;
    for body in [
        json!({ "client_name": "X", "redirect_uris": ["http://claude.ai/cb"] }),
        json!({ "client_name": "X", "redirect_uris": [] }),
        json!({ "client_name": "X", "redirect_uris": [RETURN], "grant_types": ["client_credentials"] }),
        json!({ "client_name": "x".repeat(65), "redirect_uris": [RETURN] }),
    ] {
        let response = browser()
            .post(format!("http://{addr}/register"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
    }
}
