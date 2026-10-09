//! Single sign-on with OpenID Connect (P3-04), against a stand-in provider
//! with its own keys: the server as its client, through the browser's
//! endpoints, as a browser goes through them (`/auth/oidc/begin`, the
//! provider, `/auth/oidc/callback`).

use std::net::SocketAddr;
use std::sync::Arc;

use jsonrpsee::ws_client::{HeaderMap, WsClient, WsClientBuilder};
use reqwest::StatusCode;
use serde_json::{Value, json};
use spoolframe_admin_api::{AdminApiClient, MeApiClient, Role, error_code};
use spoolframe_server::admin::{self, Admin, Origins, SetupCode, TrustedProxies};
use spoolframe_server::auth::{Auth, hash_password};
use spoolframe_server::config::OidcConfig;
use spoolframe_server::oidc::Oidc;
use spoolframe_server::passkeys::Passkeys;
use spoolframe_server::store::Store;

use crate::admin::{CODE, admin, code, state_dir};
use crate::support::idp::{Account, CLIENT_ID, CLIENT_SECRET, Document, Mischief, StandIn};

/// Where browsers reach the web UI; the provider sends them back here.
const WEB: &str = "https://media.example.org";
const PROVIDER: &str = "Pocket ID";
const PASSWORD: &str = "correct horse";

fn alice_at_the_provider() -> Account {
    Account::new("alice-sub", "alice@example.org")
}

/// A server with alice (a consumer) and root (an admin), SSO with the
/// provider at `issuer`, and its database.
struct Server {
    addr: SocketAddr,
    store: Store,
    dir: spoolframe_test_dir::TestDir,
}

async fn server(test: &str, issuer: &str) -> Server {
    server_asking(test, issuer, None).await
}

/// As [`server`], asking for `scopes`.
async fn server_asking(test: &str, issuer: &str, scopes: Option<Vec<String>>) -> Server {
    server_behind(test, issuer, scopes, TrustedProxies::default()).await
}

/// As [`server_asking`], with `proxies` believed about whom they carry.
async fn server_behind(
    test: &str,
    issuer: &str,
    scopes: Option<Vec<String>>,
    proxies: TrustedProxies,
) -> Server {
    let dir = state_dir(test).await;
    let store = Store::open(dir.as_ref()).await.unwrap();
    let phc = hash_password(PASSWORD).await.unwrap();
    store
        .add_user("alice", Some("Alice"), &phc, Role::Consumer)
        .await
        .unwrap();
    store
        .add_user("bob", None, &phc, Role::Consumer)
        .await
        .unwrap();
    store
        .add_user("root", None, &phc, Role::Admin)
        .await
        .unwrap();
    let secret = dir.as_ref().join("oidc-secret");
    std::fs::write(&secret, format!("{CLIENT_SECRET}\n")).unwrap();
    let oidc = Oidc::new(Some(WEB), None);
    oidc.set_providers(&[OidcConfig {
        name: PROVIDER.into(),
        issuer: issuer.into(),
        client_id: CLIENT_ID.into(),
        client_secret_file: Some(secret),
        scopes,
    }]);
    let origins = Origins::Exactly(WEB.into());
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            passkeys: Arc::new(Passkeys::new(&origins, &crate::admin::server_id())),
            oidc: Arc::new(oidc),
            ..admin(Auth::new(store.clone()), Some(SetupCode::new(CODE)))
        },
        origins,
        proxies,
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    Server { addr, store, dir }
}

/// A browser: its session cookie, if signed in, going through redirects
/// one at a time.
struct Browser {
    addr: SocketAddr,
    session: Option<String>,
    http: reqwest::Client,
}

/// Where a provider flow ended: the page the server sent the browser to.
#[derive(Debug)]
struct Landed {
    location: String,
    signed_in: bool,
}

/// A cookie a response set, as `name=value`.
fn set_cookie(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(&format!("{name}=")))
        .map(|v| v.split(';').next().unwrap().to_owned())
        .filter(|pair| !pair.ends_with('='))
}

impl Browser {
    fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            session: None,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn password(&mut self, user: &str) {
        let response = self
            .http
            .post(self.url("/auth/login"))
            .header("origin", WEB)
            .json(&json!({ "username": user, "password": PASSWORD }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        self.session = set_cookie(&response, "spoolframe_session");
    }

    /// Who `/auth/me` says the browser is signed in as.
    async fn me(&self) -> Option<String> {
        let mut request = self.http.get(self.url("/auth/me"));
        if let Some(cookie) = &self.session {
            request = request.header("cookie", cookie);
        }
        let response = request.send().await.unwrap();
        if response.status() != StatusCode::OK {
            return None;
        }
        let me: Value = response.json().await.unwrap();
        me["username"].as_str().map(str::to_owned)
    }

    /// The browser's socket, as the web UI opens it.
    async fn socket(&self) -> WsClient {
        let mut headers = HeaderMap::new();
        headers.insert("cookie", self.session.clone().unwrap().parse().unwrap());
        headers.insert("origin", WEB.parse().unwrap());
        WsClientBuilder::default()
            .set_headers(headers)
            .build(format!("ws://{}/jsonrpc", self.addr))
            .await
            .unwrap()
    }

    /// Begin a flow: the provider's address and the flow's cookie, or the
    /// server's refusal.
    async fn begin(&self, purpose: &str) -> Result<(String, String), (StatusCode, Value)> {
        self.begin_with(json!({ "provider": PROVIDER, "purpose": purpose, "return_to": null }))
            .await
    }

    /// [`Browser::begin`] with this body.
    async fn begin_with(&self, body: Value) -> Result<(String, String), (StatusCode, Value)> {
        let mut request = self
            .http
            .post(self.url("/auth/oidc/begin"))
            .header("origin", WEB)
            .json(&body);
        if let Some(cookie) = &self.session {
            request = request.header("cookie", cookie);
        }
        let response = request.send().await.unwrap();
        if response.status() != StatusCode::OK {
            let status = response.status();
            return Err((status, response.json().await.unwrap()));
        }
        let flow = set_cookie(&response, "spoolframe_oidc").expect("a flow cookie");
        let set = response
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            set.contains("SameSite=Lax") && set.contains("HttpOnly"),
            "{set}"
        );
        let body: Value = response.json().await.unwrap();
        Ok((body["url"].as_str().unwrap().to_owned(), flow))
    }

    /// Go to the provider: the address it sends the browser back to, on
    /// the server, as path and query.
    async fn at_provider(&self, url: &str) -> String {
        let response = self.http.get(url).send().await.unwrap();
        assert!(response.status().is_redirection(), "{response:?}");
        let to = response.headers()["location"].to_str().unwrap();
        let back = to.strip_prefix(WEB).expect("sent back to the web UI");
        assert!(back.starts_with("/auth/oidc/callback?"), "{back}");
        back.to_owned()
    }

    /// The callback, with flow cookie `flow` (if any).
    async fn back(&mut self, path: &str, flow: Option<&str>) -> Landed {
        let mut request = self.http.get(self.url(path));
        if let Some(flow) = flow {
            request = request.header("cookie", flow);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let session = set_cookie(&response, "spoolframe_session");
        let signed_in = session.is_some();
        if signed_in {
            self.session = session;
        }
        Landed {
            location: response.headers()["location"].to_str().unwrap().to_owned(),
            signed_in,
        }
    }

    /// A whole flow: begin, the provider, back.
    async fn sso(&mut self, purpose: &str) -> Landed {
        let (url, flow) = self.begin(purpose).await.expect("begun");
        let back = self.at_provider(&url).await;
        self.back(&back, Some(&flow)).await
    }
}

impl Server {
    /// Run `query` on the server's database, as a hand edit would.
    async fn sql(&self, query: &'static str, bind: &str) {
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(self.dir.as_ref().join("spoolframe.db")),
        )
        .await
        .unwrap();
        sqlx::query(query).bind(bind).execute(&pool).await.unwrap();
        pool.close().await;
    }

    /// Make `browser`'s sign-in older than the recent-sign-in window.
    async fn age(&self, browser: &Browser) {
        use spoolframe_server::auth::SessionToken;
        let token = browser.session.as_ref().unwrap().split_once('=').unwrap().1;
        let hash = SessionToken(token.to_owned()).hash();
        self.sql(
            "UPDATE sessions SET authenticated_at = authenticated_at - 3600 WHERE token_hash = ?1",
            &hash,
        )
        .await;
    }
}

#[tokio::test]
async fn alice_links_her_provider_account_signs_in_with_it_and_unlinks_it() {
    let idp = StandIn::start().await;
    let server = server("sso-link", &idp.issuer).await;
    let mut alice = Browser::new(server.addr);
    alice.password("alice").await;

    // My profile: "Link Pocket ID".
    idp.next_sign_in(alice_at_the_provider());
    let landed = alice.sso("link").await;
    assert_eq!(landed.location, "/profile?sso=linked&provider=Pocket+ID");
    let links = alice.socket().await.sso_links().await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].provider.as_deref(), Some(PROVIDER));
    assert_eq!(links[0].issuer, idp.issuer);
    assert_eq!(links[0].display.as_deref(), Some("alice@example.org"));
    assert!(alice.socket().await.who_am_i().await.unwrap().has_sso);

    // Signed out, "Continue with Pocket ID" signs her in.
    let mut later = Browser::new(server.addr);
    idp.next_sign_in(alice_at_the_provider());
    let landed = later.sso("signIn").await;
    assert_eq!(landed.location, "/");
    assert!(landed.signed_in);
    assert_eq!(later.me().await.as_deref(), Some("alice"));
    let links = later.socket().await.sso_links().await.unwrap();
    assert!(links[0].last_used_at.is_some());

    // Unlinked from My profile: it signs no one in any more.
    later
        .socket()
        .await
        .sso_unlink(idp.issuer.clone())
        .await
        .unwrap();
    assert_eq!(later.socket().await.sso_links().await.unwrap(), []);
    let mut again = Browser::new(server.addr);
    idp.next_sign_in(alice_at_the_provider());
    let landed = again.sso("signIn").await;
    assert_eq!(landed.location, "/login?sso=unlinked&provider=Pocket+ID");
    assert!(!landed.signed_in);
}

#[tokio::test]
async fn an_unlinked_account_cannot_sign_in_and_makes_no_user() {
    let idp = StandIn::start().await;
    let server = server("sso-unlinked", &idp.issuer).await;
    let users = server.store.user_count().await.unwrap();
    let mut stranger = Browser::new(server.addr);
    idp.next_sign_in(Account::new("stranger-sub", "alice@example.org"));
    let landed = stranger.sso("signIn").await;
    assert_eq!(landed.location, "/login?sso=unlinked&provider=Pocket+ID");
    assert!(!landed.signed_in);
    assert_eq!(stranger.me().await, None);
    // Not matched by email (alice's), and nobody made.
    assert_eq!(server.store.user_count().await.unwrap(), users);
}

#[tokio::test]
async fn an_id_token_that_does_not_check_out_is_refused() {
    let idp = StandIn::start().await;
    let server = server("sso-bad-tokens", &idp.issuer).await;
    let mut alice = Browser::new(server.addr);
    alice.password("alice").await;
    idp.next_sign_in(alice_at_the_provider());
    assert_eq!(
        alice.sso("link").await.location,
        "/profile?sso=linked&provider=Pocket+ID"
    );
    for mischief in [
        Mischief::WrongAudience,
        Mischief::WrongNonce,
        Mischief::WrongKey,
        Mischief::Expired,
        Mischief::WrongIssuer,
    ] {
        let mut browser = Browser::new(server.addr);
        idp.next_sign_in(alice_at_the_provider());
        idp.misbehave(mischief);
        let issued = idp.issued();
        let landed = browser.sso("signIn").await;
        assert_eq!(idp.issued(), issued + 1, "{mischief:?}: a token was issued");
        assert_eq!(
            landed.location, "/login?sso=failed&provider=Pocket+ID",
            "{mischief:?}"
        );
        assert!(!landed.signed_in, "{mischief:?}");
        assert_eq!(browser.me().await, None, "{mischief:?}");
    }
    // The same account, with a good token, still signs in.
    let mut browser = Browser::new(server.addr);
    idp.next_sign_in(alice_at_the_provider());
    assert!(browser.sso("signIn").await.signed_in);
}

#[tokio::test]
async fn an_account_linked_to_alice_cannot_be_linked_by_bob() {
    let idp = StandIn::start().await;
    let server = server("sso-taken", &idp.issuer).await;
    let mut alice = Browser::new(server.addr);
    alice.password("alice").await;
    idp.next_sign_in(alice_at_the_provider());
    alice.sso("link").await;

    let mut bob = Browser::new(server.addr);
    bob.password("bob").await;
    idp.next_sign_in(alice_at_the_provider());
    let landed = bob.sso("link").await;
    assert_eq!(landed.location, "/profile?sso=taken&provider=Pocket+ID");
    assert_eq!(bob.socket().await.sso_links().await.unwrap(), []);
    // Still alice's: it signs her in.
    let mut browser = Browser::new(server.addr);
    idp.next_sign_in(alice_at_the_provider());
    browser.sso("signIn").await;
    assert_eq!(browser.me().await.as_deref(), Some("alice"));
}

#[tokio::test]
async fn alice_with_only_her_provider_account_cannot_unlink_it() {
    let idp = StandIn::start().await;
    let server = server("sso-last", &idp.issuer).await;
    let mut alice = Browser::new(server.addr);
    alice.password("alice").await;
    idp.next_sign_in(alice_at_the_provider());
    alice.sso("link").await;
    // Her password goes (as for a user set up with SSO alone).
    server
        .sql(
            "DELETE FROM password_credentials \
             WHERE user_id = (SELECT id FROM users WHERE username = ?1)",
            "alice",
        )
        .await;
    let refused = alice.socket().await.sso_unlink(idp.issuer.clone()).await;
    assert_eq!(code(refused.unwrap_err()), error_code::LAST_SIGN_IN_METHOD);
    assert_eq!(alice.socket().await.sso_links().await.unwrap().len(), 1);
    // Not by an admin either: they would have no way in.
    let mut root = Browser::new(server.addr);
    root.password("root").await;
    let refused = root
        .socket()
        .await
        .user_sso_unlink("alice".into(), idp.issuer.clone())
        .await;
    assert_eq!(code(refused.unwrap_err()), error_code::LAST_SIGN_IN_METHOD);
}

#[tokio::test]
async fn with_the_provider_down_sso_fails_clearly_and_passwords_still_work() {
    // Nothing listens on port 9 here.
    let server = server("sso-down", "http://127.0.0.1:9").await;
    let mut browser = Browser::new(server.addr);
    let (status, body) = browser.begin("signIn").await.unwrap_err();
    assert!(status.is_server_error(), "{status}");
    assert_eq!(body["code"], error_code::UNAVAILABLE);
    assert_eq!(
        body["message"],
        "Pocket ID can't be reached right now. Sign in another way, or try again later."
    );
    browser.password("alice").await;
    assert_eq!(browser.me().await.as_deref(), Some("alice"));
}

#[tokio::test]
async fn a_callback_works_once_and_only_in_the_browser_that_began_it() {
    let idp = StandIn::start().await;
    let server = server("sso-browser", &idp.issuer).await;
    let mut alice = Browser::new(server.addr);
    alice.password("alice").await;
    idp.next_sign_in(alice_at_the_provider());
    let (url, flow) = alice.begin("link").await.unwrap();
    let back = alice.at_provider(&url).await;

    // Carried to another browser (sent as a link): nothing happens there.
    let mut elsewhere = Browser::new(server.addr);
    let landed = elsewhere.back(&back, None).await;
    assert_eq!(landed.location, "/login?sso=expired");
    let landed = elsewhere.back(&back, Some("spoolframe_oidc=guess")).await;
    assert_eq!(landed.location, "/login?sso=expired");
    assert_eq!(alice.socket().await.sso_links().await.unwrap(), []);

    // In alice's, it links; a second time, it is spent.
    let landed = alice.back(&back, Some(&flow)).await;
    assert_eq!(landed.location, "/profile?sso=linked&provider=Pocket+ID");
    let landed = alice.back(&back, Some(&flow)).await;
    assert_eq!(landed.location, "/login?sso=expired");
}

#[tokio::test]
async fn linking_needs_a_recent_sign_in_and_the_provider_can_confirm_one() {
    let idp = StandIn::start().await;
    let server = server("sso-recent", &idp.issuer).await;
    let mut alice = Browser::new(server.addr);
    alice.password("alice").await;
    idp.next_sign_in(alice_at_the_provider());
    alice.sso("link").await;
    server.age(&alice).await;

    let (_, refused) = alice.begin("link").await.unwrap_err();
    assert_eq!(refused["code"], error_code::RECENT_SIGN_IN_REQUIRED);
    let refused = alice.socket().await.sso_unlink(idp.issuer.clone()).await;
    assert_eq!(
        code(refused.unwrap_err()),
        error_code::RECENT_SIGN_IN_REQUIRED
    );

    // Another person's provider account confirms nothing.
    idp.next_sign_in(Account::new("stranger-sub", "stranger@example.org"));
    let landed = alice.sso("confirm").await;
    assert_eq!(landed.location, "/profile?sso=notyours&provider=Pocket+ID");
    let refused = alice.socket().await.sso_unlink(idp.issuer.clone()).await;
    assert_eq!(
        code(refused.unwrap_err()),
        error_code::RECENT_SIGN_IN_REQUIRED
    );

    // Her own does: the session may make sensitive changes again.
    idp.next_sign_in(alice_at_the_provider());
    let landed = alice.sso("confirm").await;
    assert_eq!(landed.location, "/profile?sso=confirmed&provider=Pocket+ID");
    alice
        .socket()
        .await
        .sso_unlink(idp.issuer.clone())
        .await
        .unwrap();
}

/// A-15: a confirmation asks the provider to sign the account in again
/// (`prompt=login`, `max_age=0`), as the password does, rather than take
/// a sign-in it remembers; signing in and linking do not ask.
#[tokio::test]
async fn confirming_asks_the_provider_to_sign_in_again() {
    let idp = StandIn::start().await;
    let server = server("sso-confirm-asks", &idp.issuer).await;
    let mut alice = Browser::new(server.addr);
    alice.password("alice").await;
    idp.next_sign_in(alice_at_the_provider());
    alice.sso("link").await;
    assert_eq!(idp.last_asked("prompt"), None);
    assert_eq!(idp.last_asked("max_age"), None);
    let mut later = Browser::new(server.addr);
    idp.next_sign_in(alice_at_the_provider());
    assert!(later.sso("signIn").await.signed_in);
    assert_eq!(idp.last_asked("prompt"), None);
    assert_eq!(idp.last_asked("max_age"), None);

    server.age(&alice).await;
    idp.next_sign_in(alice_at_the_provider());
    let landed = alice.sso("confirm").await;
    assert_eq!(landed.location, "/profile?sso=confirmed&provider=Pocket+ID");
    assert_eq!(idp.last_asked("prompt").as_deref(), Some("login"));
    assert_eq!(idp.last_asked("max_age").as_deref(), Some("0"));
}

/// A-15: a provider that answers a confirmation with a sign-in from before
/// it began (its `auth_time`) confirms nothing. Signing in takes it.
#[tokio::test]
async fn a_provider_sign_in_from_before_the_confirmation_confirms_nothing() {
    let idp = StandIn::start().await;
    let server = server("sso-confirm-stale", &idp.issuer).await;
    let mut alice = Browser::new(server.addr);
    alice.password("alice").await;
    idp.next_sign_in(alice_at_the_provider());
    alice.sso("link").await;
    server.age(&alice).await;

    idp.next_sign_in(alice_at_the_provider());
    idp.misbehave(Mischief::SignedInLongAgo);
    let landed = alice.sso("confirm").await;
    assert_eq!(landed.location, "/profile?sso=failed&provider=Pocket+ID");
    let refused = alice.socket().await.sso_unlink(idp.issuer.clone()).await;
    assert_eq!(
        code(refused.unwrap_err()),
        error_code::RECENT_SIGN_IN_REQUIRED
    );

    let mut later = Browser::new(server.addr);
    idp.next_sign_in(alice_at_the_provider());
    idp.misbehave(Mischief::SignedInLongAgo);
    assert!(later.sso("signIn").await.signed_in);
}

#[tokio::test]
async fn an_admin_sees_and_removes_a_user_s_link_and_cannot_make_one() {
    let idp = StandIn::start().await;
    let server = server("sso-admin", &idp.issuer).await;
    let mut alice = Browser::new(server.addr);
    alice.password("alice").await;
    idp.next_sign_in(alice_at_the_provider());
    alice.sso("link").await;

    let mut root = Browser::new(server.addr);
    root.password("root").await;
    let admin_api = root.socket().await;
    let links = admin_api.user_sso_links("alice".into()).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].display.as_deref(), Some("alice@example.org"));
    // Alice is no admin.
    let forbidden = alice.socket().await.user_sso_links("bob".into()).await;
    assert_eq!(code(forbidden.unwrap_err()), error_code::FORBIDDEN);
    admin_api
        .user_sso_unlink("alice".into(), idp.issuer.clone())
        .await
        .unwrap();
    assert_eq!(alice.socket().await.sso_links().await.unwrap(), []);
    // A link goes to the session that begins it: root's to root, never to
    // someone else. There is no admin method to link.
    idp.next_sign_in(alice_at_the_provider());
    root.sso("link").await;
    assert_eq!(alice.socket().await.sso_links().await.unwrap(), []);
    assert_eq!(
        admin_api.user_sso_links("root".into()).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn a_cancelled_sign_in_at_the_provider_signs_no_one_in() {
    let idp = StandIn::start().await;
    let server = server("sso-cancel", &idp.issuer).await;
    let mut browser = Browser::new(server.addr);
    let (url, flow) = browser.begin("signIn").await.unwrap();
    // The provider's page: deny.
    let page = reqwest::get(&url).await.unwrap().text().await.unwrap();
    let request = page
        .split("name=\"request\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let denied = browser
        .http
        .post(format!("{}/approve", idp.issuer))
        .form(&[("request", request.as_str()), ("deny", "1")])
        .send()
        .await
        .unwrap();
    let to = denied.headers()["location"].to_str().unwrap();
    let back = to.strip_prefix(WEB).unwrap().to_owned();
    let landed = browser.back(&back, Some(&flow)).await;
    assert_eq!(landed.location, "/login?sso=cancelled");
    assert!(!landed.signed_in);
}

#[tokio::test]
async fn without_providers_there_is_no_sso() {
    let (addr, _dir) = crate::admin::server("sso-none").await;
    let site: Value = reqwest::get(format!("http://{addr}/auth/site"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(site["sso"], json!([]));
    let refused = reqwest::Client::new()
        .post(format!("http://{addr}/auth/oidc/begin"))
        .json(&json!({ "provider": PROVIDER, "purpose": "signIn", "return_to": null }))
        .send()
        .await
        .unwrap();
    let body: Value = refused.json().await.unwrap();
    assert_eq!(body["code"], error_code::NOT_CONFIGURED);
}

#[tokio::test]
async fn the_site_names_the_providers_for_the_login_page() {
    let idp = StandIn::start().await;
    let server = server("sso-site", &idp.issuer).await;
    let site: Value = reqwest::get(format!("http://{}/auth/site", server.addr))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(site["sso"], json!([PROVIDER]));
}

/// Everything about the provider comes from its discovery document: one
/// that names another issuer than the configured one is refused, saying
/// both (OpenID Connect Discovery requires them equal).
#[tokio::test]
async fn a_discovery_document_for_another_issuer_is_refused_naming_both() {
    let idp = StandIn::start().await;
    idp.describe(Document {
        issuer: Some("https://impostor.example.org".into()),
        ..Document::default()
    });
    let server = server("sso-impostor", &idp.issuer).await;
    let (_, refused) = Browser::new(server.addr).begin("signIn").await.unwrap_err();
    assert_eq!(refused["code"], error_code::NOT_CONFIGURED);
    let message = refused["message"].as_str().unwrap();
    assert!(
        message.contains("https://impostor.example.org"),
        "{message}"
    );
    assert!(message.contains(&idp.issuer), "{message}");
}

#[tokio::test]
async fn a_provider_that_offers_only_plain_pkce_is_refused() {
    let idp = StandIn::start().await;
    idp.describe(Document {
        pkce: vec!["plain"],
        ..Document::default()
    });
    let server = server("sso-plain-pkce", &idp.issuer).await;
    let (_, refused) = Browser::new(server.addr).begin("signIn").await.unwrap_err();
    assert_eq!(refused["code"], error_code::NOT_CONFIGURED);
    assert!(refused["message"].as_str().unwrap().contains("S256"));
}

/// The token endpoint is called as the discovery document says: basic
/// where offered, else the client's id and secret in the body.
#[tokio::test]
async fn the_client_authenticates_as_the_provider_offers() {
    let idp = StandIn::start().await;
    let first = server("sso-token-auth", &idp.issuer).await;
    let mut alice = Browser::new(first.addr);
    alice.password("alice").await;
    idp.next_sign_in(alice_at_the_provider());
    // Basic and post offered, and the stand-in accepts either.
    assert_eq!(
        alice.sso("link").await.location,
        "/profile?sso=linked&provider=Pocket+ID"
    );
    for (offered, works) in [
        (vec!["client_secret_post"], true),
        (vec!["private_key_jwt"], false),
    ] {
        idp.describe(Document {
            token_auth: offered.clone(),
            ..Document::default()
        });
        // The document is kept a minute: a new server reads it fresh.
        let fresh = server(&format!("sso-token-auth-{}", offered[0]), &idp.issuer).await;
        fresh
            .sql(
                "INSERT INTO external_identities (user_id, issuer, subject, linked_at) \
                 VALUES ((SELECT id FROM users WHERE username = 'alice'), ?1, 'alice-sub', 0)",
                &idp.issuer,
            )
            .await;
        let mut browser = Browser::new(fresh.addr);
        idp.next_sign_in(alice_at_the_provider());
        if works {
            assert!(browser.sso("signIn").await.signed_in, "{offered:?}");
        } else {
            let (_, refused) = browser.begin("signIn").await.unwrap_err();
            assert_eq!(refused["code"], error_code::NOT_CONFIGURED, "{offered:?}");
        }
    }
}

/// The account is shown by its username at the provider, else its email;
/// it is matched by neither.
#[tokio::test]
async fn a_link_shows_the_provider_s_username_and_asks_the_configured_scopes() {
    let idp = StandIn::start().await;
    let server = server_asking(
        "sso-scopes",
        &idp.issuer,
        Some(vec!["openid".into(), "email".into()]),
    )
    .await;
    let mut alice = Browser::new(server.addr);
    alice.password("alice").await;
    idp.next_sign_in(alice_at_the_provider().named("alice.w"));
    alice.sso("link").await;
    assert_eq!(idp.last_scope().as_deref(), Some("openid email"));
    let links = alice.socket().await.sso_links().await.unwrap();
    assert_eq!(links[0].display.as_deref(), Some("alice.w"));
    // The same subject with another username and email is still hers.
    let mut later = Browser::new(server.addr);
    idp.next_sign_in(Account::new("alice-sub", "new@example.org").named("someone"));
    later.sso("signIn").await;
    assert_eq!(later.me().await.as_deref(), Some("alice"));
}

/// Where the device page a TV's QR code opens is (P3-07).
const DEVICE_PAGE: &str = "/auth/device?code=BCDF-GHJK";

impl Browser {
    /// A sign-in flow begun on the page at `return_to`, through the
    /// provider and back, the callback's address ending in `extra`.
    async fn sign_in_from(&mut self, return_to: &str, extra: &str) -> Landed {
        let (url, flow) = self
            .begin_with(json!({
                "provider": PROVIDER,
                "purpose": "signIn",
                "return_to": return_to,
            }))
            .await
            .expect("begun");
        let back = self.at_provider(&url).await;
        self.back(&format!("{back}{extra}"), Some(&flow)).await
    }
}

#[tokio::test]
async fn a_sign_in_begun_on_the_device_page_returns_to_it() {
    let idp = StandIn::start().await;
    let server = server("sso-return", &idp.issuer).await;
    let mut alice = Browser::new(server.addr);
    alice.password("alice").await;
    idp.next_sign_in(alice_at_the_provider());
    alice.sso("link").await;

    // Signed out on a phone, from the device page: back to it, code and
    // all, signed in. Where to go is what the begin said: a callback
    // that names another place is not listened to.
    let mut phone = Browser::new(server.addr);
    idp.next_sign_in(alice_at_the_provider());
    let landed = phone
        .sign_in_from(DEVICE_PAGE, "&return_to=%2F%2Fevil.example")
        .await;
    assert_eq!(landed.location, DEVICE_PAGE);
    assert!(landed.signed_in);
    assert_eq!(phone.me().await.as_deref(), Some("alice"));

    // An account that is not linked: back to the page too, saying so
    // after its own query.
    let mut stranger = Browser::new(server.addr);
    idp.next_sign_in(Account::new("stranger-sub", "s@example.org"));
    let landed = stranger.sign_in_from(DEVICE_PAGE, "").await;
    assert_eq!(
        landed.location,
        "/auth/device?code=BCDF-GHJK&sso=unlinked&provider=Pocket+ID"
    );
    assert!(!landed.signed_in);

    // The login page's own sign-in still lands on the start page.
    let mut desk = Browser::new(server.addr);
    idp.next_sign_in(alice_at_the_provider());
    assert_eq!(desk.sso("signIn").await.location, "/");
}

#[tokio::test]
async fn a_place_to_return_to_off_the_server_is_refused() {
    let idp = StandIn::start().await;
    let server = server("sso-return-hostile", &idp.issuer).await;
    let browser = Browser::new(server.addr);
    for hostile in [
        "//evil.example/auth/device",
        "https://evil.example/",
        "javascript:alert(1)",
        "/\\evil.example",
        "\\\\evil.example",
        "/\t/evil.example",
        "/auth/device?code=X\r\nSet-Cookie: a=b",
        "auth/device",
        "",
        "/../..//evil.example",
    ] {
        let refused = browser
            .begin_with(json!({
                "provider": PROVIDER,
                "purpose": "signIn",
                "return_to": hostile,
            }))
            .await;
        let (status, body) = refused.expect_err(&format!("{hostile:?} was taken"));
        assert_eq!(status, StatusCode::BAD_REQUEST, "{hostile:?}");
        assert_eq!(body["code"], error_code::INVALID_INPUT, "{hostile:?}");
    }
    // Only a sign-in returns anywhere: a link goes back to My profile.
    let mut alice = Browser::new(server.addr);
    alice.password("alice").await;
    let (status, _) = alice
        .begin_with(json!({
            "provider": PROVIDER,
            "purpose": "link",
            "return_to": DEVICE_PAGE,
        }))
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// The web UI always says where to return to, `null` for the start page
/// (PX-15): a begin that leaves the key out is refused, not read as none.
#[tokio::test]
async fn a_begin_that_leaves_return_to_out_is_refused() {
    let idp = StandIn::start().await;
    let server = server("sso-return-missing", &idp.issuer).await;
    let browser = Browser::new(server.addr);
    let (status, body) = browser
        .begin_with(json!({ "provider": PROVIDER, "purpose": "signIn" }))
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], error_code::INVALID_INPUT);
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("missing field `return_to`")),
        "{body}"
    );
}

/// PX-09 finding 16 (A-6): anyone may begin a sign-in, so one address may
/// hold only a few flows, and the table stays open to everyone else.
#[tokio::test]
async fn one_address_cannot_begin_every_sso_flow() {
    let idp = StandIn::start().await;
    let server = server_behind(
        "sso-begin-source",
        &idp.issuer,
        None,
        TrustedProxies::new(&["127.0.0.1"]).unwrap(),
    )
    .await;
    let addr = server.addr;
    let begin_from = |caller: &'static str| async move {
        reqwest::Client::new()
            .post(format!("http://{addr}/auth/oidc/begin"))
            .header("origin", WEB)
            .header("x-forwarded-for", caller)
            .json(&json!({ "provider": PROVIDER, "purpose": "signIn", "return_to": null }))
            .send()
            .await
            .unwrap()
            .status()
    };
    for _ in 0..spoolframe_server::oidc::MAX_FLOWS_PER_SOURCE {
        assert_eq!(begin_from("203.0.113.1").await, StatusCode::OK);
    }
    assert_ne!(begin_from("203.0.113.1").await, StatusCode::OK);
    assert_eq!(begin_from("203.0.113.2").await, StatusCode::OK);
}
