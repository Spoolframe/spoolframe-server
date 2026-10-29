//! A stand-in OpenID Connect provider (P3-04): discovery, its keys, an
//! authorization endpoint and a token endpoint that issues ID tokens
//! signed with a key of its own. Enough of a provider for the server to be
//! a real client of it, and for a test to make it misbehave.
//!
//! Who signs in at it is the test's to say ([`StandIn::next_sign_in`]):
//! the authorization endpoint then answers at once, as a provider does for
//! someone already signed in there. Without one it shows a page to pick
//! who signs in, for a person or a browser test.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use openidconnect::core::{
    CoreIdToken, CoreIdTokenClaims, CoreJsonWebKeySet, CoreJwsSigningAlgorithm,
    CoreRsaPrivateSigningKey,
};
use openidconnect::{
    Audience, EmptyAdditionalClaims, EndUserEmail, EndUserUsername, IssuerUrl, JsonWebKeyId, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, PrivateSigningKey, StandardClaims, SubjectIdentifier,
};
use serde_json::json;

/// The key the stand-in signs with, and another it does not publish.
const KEY: &str = include_str!("idp-key.pem");
const OTHER_KEY: &str = include_str!("idp-other-key.pem");

/// The client the server is at the stand-in.
pub const CLIENT_ID: &str = "spoolframe";
pub const CLIENT_SECRET: &str = "s3cr3t-client";

/// What the stand-in gets wrong in its next ID token, on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mischief {
    /// For another client.
    WrongAudience,
    /// With a nonce the server never sent.
    WrongNonce,
    /// Signed with a key the stand-in does not publish.
    WrongKey,
    /// Expired an hour ago.
    Expired,
    /// Saying it is from another issuer.
    WrongIssuer,
    /// Saying the account signed in at the provider an hour ago
    /// (`auth_time`), as one that did not ask again would.
    SignedInLongAgo,
}

/// Who signs in at the stand-in: a subject, and an email to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub subject: String,
    pub email: String,
    /// `preferred_username`, if the provider says one.
    pub username: Option<String>,
}

impl Account {
    pub fn new(subject: &str, email: &str) -> Self {
        Self {
            subject: subject.into(),
            email: email.into(),
            username: None,
        }
    }

    pub fn named(mut self, username: &str) -> Self {
        self.username = Some(username.into());
        self
    }
}

/// How the stand-in describes itself in its discovery document, where a
/// test makes it differ from the usual.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    /// The issuer it names; its own if `None`.
    pub issuer: Option<String>,
    /// The token endpoint's client authentication methods; it accepts
    /// only these.
    pub token_auth: Vec<&'static str>,
    pub pkce: Vec<&'static str>,
}

impl Default for Document {
    /// What Pocket ID says of itself.
    fn default() -> Self {
        Self {
            issuer: None,
            token_auth: vec!["client_secret_basic", "client_secret_post"],
            pkce: vec!["S256", "plain"],
        }
    }
}

/// An authorization request waiting for its code to be exchanged.
#[derive(Debug, Clone)]
struct Grant {
    account: Account,
    nonce: Option<String>,
    challenge: String,
    redirect_uri: String,
    client_id: String,
    /// Whether the request asked how long ago the account signed in
    /// (`max_age`): its ID token then says when (`auth_time`), as OpenID
    /// Connect Core §3.1.2.1 requires.
    max_age: bool,
}

#[derive(Default)]
struct Inner {
    next: Option<Account>,
    mischief: Option<Mischief>,
    codes: HashMap<String, Grant>,
    /// Authorization requests shown as a page, by id.
    pending: HashMap<String, HashMap<String, String>>,
    /// How many tokens it issued, for tests that check none was.
    issued: u32,
    document: Document,
    /// The `scope` of the last authorization request.
    scope: Option<String>,
    /// The last authorization request's parameters.
    last: HashMap<String, String>,
}

/// A running stand-in provider.
#[derive(Clone)]
pub struct StandIn {
    pub issuer: String,
    inner: Arc<Mutex<Inner>>,
}

struct Shared {
    issuer: String,
    inner: Arc<Mutex<Inner>>,
    key: CoreRsaPrivateSigningKey,
    other: CoreRsaPrivateSigningKey,
}

impl StandIn {
    /// A stand-in on loopback, at a port of its own.
    pub async fn start() -> Self {
        Self::start_on("127.0.0.1:0".parse().unwrap()).await
    }

    /// A stand-in at `addr`.
    pub async fn start_on(addr: SocketAddr) -> Self {
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let inner = Arc::new(Mutex::new(Inner::default()));
        let shared = Arc::new(Shared {
            issuer: issuer.clone(),
            inner: inner.clone(),
            key: CoreRsaPrivateSigningKey::from_pem(KEY, Some(JsonWebKeyId::new("k1".into())))
                .unwrap(),
            other: CoreRsaPrivateSigningKey::from_pem(
                OTHER_KEY,
                Some(JsonWebKeyId::new("k1".into())),
            )
            .unwrap(),
        });
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/authorize", get(authorize))
            .route("/approve", post(approve))
            .route("/token", post(token))
            .with_state(shared);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { issuer, inner }
    }

    /// The next authorization signs `account` in, at once.
    pub fn next_sign_in(&self, account: Account) {
        self.inner.lock().next = Some(account);
    }

    /// The next ID token is wrong in this way.
    pub fn misbehave(&self, mischief: Mischief) {
        self.inner.lock().mischief = Some(mischief);
    }

    /// How many ID tokens it has issued.
    pub fn issued(&self) -> u32 {
        self.inner.lock().issued
    }

    /// Describe itself as `document` from now on.
    pub fn describe(&self, document: Document) {
        self.inner.lock().document = document;
    }

    /// The scopes the last authorization request asked for.
    pub fn last_scope(&self) -> Option<String> {
        self.inner.lock().scope.clone()
    }

    /// The parameter `name` of the last authorization request, if it had
    /// one.
    pub fn last_asked(&self, name: &str) -> Option<String> {
        self.inner.lock().last.get(name).cloned()
    }
}

fn random() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the OS gives random bytes");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn discovery(State(s): State<Arc<Shared>>) -> Response {
    let i = &s.issuer;
    let document = s.inner.lock().document.clone();
    axum::Json(json!({
        "issuer": document.issuer.as_deref().unwrap_or(i),
        "authorization_endpoint": format!("{i}/authorize"),
        "token_endpoint": format!("{i}/token"),
        "jwks_uri": format!("{i}/jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
        "scopes_supported": ["openid", "profile", "email"],
        "token_endpoint_auth_methods_supported": document.token_auth,
        "code_challenge_methods_supported": document.pkce,
    }))
    .into_response()
}

async fn jwks(State(s): State<Arc<Shared>>) -> Response {
    axum::Json(CoreJsonWebKeySet::new(vec![s.key.as_verification_key()])).into_response()
}

fn query(req: &Request) -> HashMap<String, String> {
    url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes())
        .into_owned()
        .collect()
}

/// A code for `account`, sent back to the client's redirect URI.
fn grant(s: &Shared, params: &HashMap<String, String>, account: Account) -> Response {
    let (Some(redirect_uri), Some(state), Some(challenge)) = (
        params.get("redirect_uri"),
        params.get("state"),
        params.get("code_challenge"),
    ) else {
        return (
            StatusCode::BAD_REQUEST,
            "redirect_uri, state and PKCE are required",
        )
            .into_response();
    };
    if params.get("code_challenge_method").map(String::as_str) != Some("S256") {
        return (StatusCode::BAD_REQUEST, "S256 only").into_response();
    }
    let code = random();
    s.inner.lock().codes.insert(
        code.clone(),
        Grant {
            account,
            nonce: params.get("nonce").cloned(),
            challenge: challenge.clone(),
            redirect_uri: redirect_uri.clone(),
            client_id: params.get("client_id").cloned().unwrap_or_default(),
            max_age: params.contains_key("max_age"),
        },
    );
    let mut to = url::Url::parse(redirect_uri).unwrap();
    to.query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", state);
    Redirect::to(to.as_str()).into_response()
}

async fn authorize(State(s): State<Arc<Shared>>, req: Request) -> Response {
    let params = query(&req);
    if params.get("response_type").map(String::as_str) != Some("code") {
        return (StatusCode::BAD_REQUEST, "response_type=code only").into_response();
    }
    s.inner.lock().scope = params.get("scope").cloned();
    s.inner.lock().last = params.clone();
    let next = s.inner.lock().next.take();
    if let Some(account) = next {
        return grant(&s, &params, account);
    }
    // Nobody named: a page to pick who signs in.
    let id = random();
    s.inner.lock().pending.insert(id.clone(), params.clone());
    let button = |sub: &str, email: &str, label: &str| {
        format!(
            r#"<form method="post" action="/approve"><input type="hidden" name="request" value="{id}"><input type="hidden" name="sub" value="{sub}"><input type="hidden" name="email" value="{email}"><button>{label}</button></form>"#
        )
    };
    let deny = format!(
        r#"<form method="post" action="/approve"><input type="hidden" name="request" value="{id}"><input type="hidden" name="deny" value="1"><button>Deny</button></form>"#
    );
    Html(format!(
        "<!doctype html><title>Stand-in provider</title><h1>Stand-in provider</h1>{}{}{deny}",
        button(
            "alice-sub",
            "alice@example.org",
            "Sign in as alice@example.org"
        ),
        button(
            "stranger-sub",
            "stranger@example.org",
            "Sign in as stranger@example.org"
        ),
    ))
    .into_response()
}

async fn approve(State(s): State<Arc<Shared>>, body: axum::body::Bytes) -> Response {
    let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    let Some(params) = form
        .get("request")
        .and_then(|id| s.inner.lock().pending.remove(id))
    else {
        return (StatusCode::BAD_REQUEST, "no such request").into_response();
    };
    if form.contains_key("deny") {
        let mut to = url::Url::parse(&params["redirect_uri"]).unwrap();
        to.query_pairs_mut()
            .append_pair("error", "access_denied")
            .append_pair("state", &params["state"]);
        return Redirect::to(to.as_str()).into_response();
    }
    let account = Account::new(&form["sub"], &form["email"]);
    grant(&s, &params, account)
}

/// The client's id and secret from HTTP Basic auth.
fn basic(headers: &HeaderMap) -> Option<(String, String)> {
    use base64::Engine;
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value.strip_prefix("Basic ")?)
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (id, secret) = text.split_once(':')?;
    let unescape = |s: &str| {
        url::form_urlencoded::parse(format!("x={s}").as_bytes())
            .next()
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default()
    };
    Some((unescape(id), unescape(secret)))
}

fn oauth_error(error: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({ "error": error })),
    )
        .into_response()
}

async fn token(
    State(s): State<Arc<Shared>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    let accepts = s.inner.lock().document.token_auth.clone();
    let client = (CLIENT_ID.to_owned(), CLIENT_SECRET.to_owned());
    let by_basic =
        accepts.contains(&"client_secret_basic") && basic(&headers) == Some(client.clone());
    let by_post = accepts.contains(&"client_secret_post")
        && form.get("client_id") == Some(&client.0)
        && form.get("client_secret") == Some(&client.1);
    if !(by_basic || by_post) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({ "error": "invalid_client" })),
        )
            .into_response();
    }
    if form.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return oauth_error("unsupported_grant_type");
    }
    // A code is good once.
    let Some(grant) = form
        .get("code")
        .and_then(|c| s.inner.lock().codes.remove(c))
    else {
        return oauth_error("invalid_grant");
    };
    if form.get("redirect_uri") != Some(&grant.redirect_uri) || grant.client_id != CLIENT_ID {
        return oauth_error("invalid_grant");
    }
    let Some(verifier) = form.get("code_verifier") else {
        return oauth_error("invalid_grant");
    };
    let challenge =
        PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(verifier.clone()));
    if challenge.as_str() != grant.challenge {
        return oauth_error("invalid_grant");
    }
    let mischief = {
        let mut inner = s.inner.lock();
        inner.issued += 1;
        inner.mischief.take()
    };
    let now = chrono::Utc::now();
    let (issued_at, expires) = if mischief == Some(Mischief::Expired) {
        (
            now - chrono::Duration::hours(2),
            now - chrono::Duration::hours(1),
        )
    } else {
        (now, now + chrono::Duration::minutes(5))
    };
    let issuer = if mischief == Some(Mischief::WrongIssuer) {
        "http://127.0.0.1:1/elsewhere".to_owned()
    } else {
        s.issuer.clone()
    };
    let audience = if mischief == Some(Mischief::WrongAudience) {
        "another-client"
    } else {
        CLIENT_ID
    };
    let nonce = if mischief == Some(Mischief::WrongNonce) {
        Some("not-the-one".to_owned())
    } else {
        grant.nonce
    };
    let claims = CoreIdTokenClaims::new(
        IssuerUrl::new(issuer).unwrap(),
        vec![Audience::new(audience.into())],
        expires,
        issued_at,
        StandardClaims::new(SubjectIdentifier::new(grant.account.subject))
            .set_email(Some(EndUserEmail::new(grant.account.email)))
            .set_preferred_username(grant.account.username.map(EndUserUsername::new)),
        EmptyAdditionalClaims {},
    )
    .set_nonce(nonce.map(Nonce::new))
    .set_auth_time(if mischief == Some(Mischief::SignedInLongAgo) {
        Some(now - chrono::Duration::hours(1))
    } else {
        grant.max_age.then_some(now)
    });
    let key = if mischief == Some(Mischief::WrongKey) {
        &s.other
    } else {
        &s.key
    };
    let id_token = CoreIdToken::new(
        claims,
        key,
        CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
        None,
        None,
    )
    .unwrap();
    axum::Json(json!({
        "access_token": random(),
        "token_type": "Bearer",
        "expires_in": 300,
        "id_token": id_token.to_string(),
    }))
    .into_response()
}
