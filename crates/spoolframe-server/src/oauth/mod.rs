//! The server as its own OAuth 2.1 authorization server (PX-42), for MCP
//! clients: dynamic client registration (RFC 7591), the authorization code
//! with PKCE (S256 only), and refresh tokens bound to the client.
//!
//! A client registers itself, sends its user's browser to `/authorize`,
//! and the user signs in to the web UI as they always do and consents on
//! its page to some of the scopes asked for. The code goes back to the
//! client's registered address; the client trades it, with the PKCE
//! verifier, for an access token (an hour) and a refresh token (30 days
//! from its last use, used once). Codes and tokens are stored hashed.
//!
//! A request that is broken (an unknown client, an address it did not
//! register, no PKCE) is answered on the server's own page, and nothing is
//! sent to any address: a redirect there would vouch for it. The HTTP side
//! is the admin listener's; this is the rules, with no HTTP in them.

use sha2::Digest;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::store::oauth::{ClientRecord, NewGrant, NewTokens, OauthAccess, Redeemed, Refreshed};
use crate::store::users::LocalUserId;
use crate::store::{Store, StoreError};

pub mod redirects;

use redirects::RedirectPattern;

/// How long an access token works.
pub const ACCESS_TOKEN: Duration = Duration::from_secs(3600);
/// How long a refresh token works after its last use (its rotation).
pub const REFRESH_TOKEN: Duration = Duration::from_secs(30 * 24 * 3600);
/// How long a code waits to be traded for tokens.
pub const CODE: Duration = Duration::from_secs(60);
/// How long an authorization request waits for its user to consent.
pub const PENDING: Duration = Duration::from_secs(600);
/// How long a client nobody granted anything is kept.
pub const UNUSED_CLIENT: Duration = Duration::from_secs(24 * 3600);
/// The longest name a client may register, in characters.
pub const MAX_CLIENT_NAME: usize = 64;
/// The most redirect URIs one client registers.
pub const MAX_REDIRECT_URIS: usize = 5;
/// The most authorization requests waiting for consent at once: a flood
/// of `/authorize` holds no more memory than this.
const MAX_PENDING: usize = 1000;
/// The most clients one address registers in [`REGISTRATION_WINDOW`].
pub const MAX_REGISTRATIONS: u32 = 20;
pub const REGISTRATION_WINDOW: Duration = Duration::from_secs(3600);
/// The most addresses whose registrations are counted at once.
const MAX_REGISTRANTS: usize = 10_000;
/// What every token starts with: a Spoolframe OAuth token, told apart
/// from a session token by eye and by secret scanners.
const TOKEN_PREFIX: &str = "sfo_";

/// What a token lets an MCP client do, as OAuth names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Search, item details, what's new, the user's watch history.
    LibraryRead,
    /// See the user's playlists and what is in them, and change nothing.
    PlaylistsRead,
    /// Make, fill and edit playlists; mark watched or unwatched. Covers
    /// [`Scope::PlaylistsRead`]: an app that changes playlists reads them.
    PlaylistsWrite,
    /// Users, sessions, playbacks and the log: admins only.
    Admin,
}

impl Scope {
    /// Every scope the server has, in the order consent lists them.
    pub const ALL: [Scope; 4] = [
        Scope::LibraryRead,
        Scope::PlaylistsRead,
        Scope::PlaylistsWrite,
        Scope::Admin,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::LibraryRead => "library:read",
            Scope::PlaylistsRead => "playlists:read",
            Scope::PlaylistsWrite => "playlists:write",
            Scope::Admin => "admin",
        }
    }

    /// The scope called `name`, if the server has it. `playback` it does
    /// not have yet (PX-05).
    pub fn parse(name: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|s| s.as_str() == name)
    }
}

/// `scopes` as OAuth writes them: space-separated.
pub fn scope_string(scopes: &[Scope]) -> String {
    scopes
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The known scopes in `text` (space-separated), each once, in
/// [`Scope::ALL`]'s order; the rest dropped.
pub fn parse_scopes(text: &str) -> Vec<Scope> {
    let mut scopes: Vec<Scope> = text.split_whitespace().filter_map(Scope::parse).collect();
    scopes.sort();
    scopes.dedup();
    scopes
}

/// The scopes consent offers for a request asking for `requested`: those
/// the server has, and `admin` only to an admin. A scope the server does
/// not have is dropped, as OAuth lets a server grant less than asked. A
/// request that names none of the server's gets `library:read`.
pub fn offered(requested: &str, admin: bool) -> Vec<Scope> {
    let scopes: Vec<Scope> = parse_scopes(requested)
        .into_iter()
        .filter(|s| admin || *s != Scope::Admin)
        .collect();
    if scopes.is_empty() {
        vec![Scope::LibraryRead]
    } else {
        scopes
    }
}

/// Whether `verifier` is a PKCE code verifier: 43 to 128 of the
/// unreserved characters (RFC 7636 §4.1).
pub fn valid_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// Whether `challenge` is an S256 challenge: a SHA-256, base64url
/// without padding.
fn valid_challenge(challenge: &str) -> bool {
    challenge.len() == 43
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// Whether `verifier` is the one `challenge` was made from (S256).
/// Compared through hashes, so the time taken says nothing about how much
/// of it was right.
pub fn pkce_matches(verifier: &str, challenge: &str) -> bool {
    use base64::Engine;
    let made = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    blake3::hash(made.as_bytes()) == blake3::hash(challenge.as_bytes())
}

/// Whether `host` is this machine's loopback: what a native app's own
/// listener is at (RFC 8252 §7.3).
fn loopback(host: Option<url::Host<&str>>) -> bool {
    match host {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Why a redirect URI, or an allowed pattern of them, cannot return to
/// `scheme` on `host`, if it cannot: `https` anywhere, or plain `http` on
/// loopback only (a native app's own listener); no other scheme. Worded to
/// follow the URI or pattern.
fn redirect_scheme_refused(scheme: &str, host: Option<url::Host<&str>>) -> Option<String> {
    match scheme {
        "https" if host.is_some() => None,
        "https" => Some("has no host".into()),
        "http" if loopback(host) => None,
        "http" => {
            Some("is plain http, which is only for loopback (127.0.0.1, [::1] or localhost)".into())
        }
        other => Some(format!("has the scheme {other:?}; use https")),
    }
}

/// `uri` as a redirect URI a client may register: `https` anywhere, or
/// plain `http` on loopback only (`redirect_scheme_refused`); never a
/// fragment, nor credentials.
pub fn check_redirect(uri: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(uri).map_err(|e| format!("{uri:?} is not a URL: {e}"))?;
    if url.fragment().is_some() {
        return Err(format!("{uri:?} has a fragment"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("{uri:?} carries credentials"));
    }
    match redirect_scheme_refused(url.scheme(), url.host()) {
        None => Ok(url),
        Some(why) => Err(format!("{uri:?} {why}")),
    }
}

/// Whether `given` is the registered `registered`: the same string, or
/// for a loopback address the same but for the port, which a native app
/// picks when it listens (RFC 8252 §7.3).
pub fn redirect_matches(registered: &str, given: &str) -> bool {
    if registered == given {
        return true;
    }
    let (Ok(mut registered), Ok(mut given)) = (url::Url::parse(registered), url::Url::parse(given))
    else {
        return false;
    };
    if registered.scheme() != "http" || !loopback(registered.host()) {
        return false;
    }
    // Both ports cleared; the rest must still be the same.
    let cleared = registered.set_port(None).is_ok() && given.set_port(None).is_ok();
    cleared && registered == given
}

/// The host a redirect URI returns to, as consent and My profile show it
/// (`claude.ai`).
pub fn return_host(redirect_uri: &str) -> String {
    url::Url::parse(redirect_uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| redirect_uri.to_owned())
}

/// A random secret: 32 bytes, hex.
fn secret() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS gives random bytes");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What the store keys a code or a token by.
pub fn hash(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}

/// MCP's live settings, read at every request: whether MCP and its OAuth
/// endpoints are on (`mcp.enabled`; off, they are not found), and the
/// apps allowed to sign in (`mcp.allowed_redirects`).
#[derive(Debug, Clone)]
pub struct McpSwitch {
    on: Arc<AtomicBool>,
    allowed: Arc<parking_lot::RwLock<Arc<[RedirectPattern]>>>,
}

impl Default for McpSwitch {
    /// Off, with the default apps allowed.
    fn default() -> Self {
        Self::new(false)
    }
}

impl McpSwitch {
    /// On or off, with the default apps allowed.
    pub fn new(on: bool) -> Self {
        Self {
            on: Arc::new(AtomicBool::new(on)),
            allowed: Arc::new(parking_lot::RwLock::new(redirects::defaults().into())),
        }
    }

    pub fn on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    pub fn set(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
    }

    /// The apps allowed to sign in now.
    pub fn allowed(&self) -> Arc<[RedirectPattern]> {
        self.allowed.read().clone()
    }

    pub fn set_allowed(&self, allowed: Vec<RedirectPattern>) {
        *self.allowed.write() = allowed.into();
    }

    /// Whether an app returning to `redirect_uri` may sign in now.
    pub fn redirect_allowed(&self, redirect_uri: &str) -> bool {
        redirects::allowed(&self.allowed.read(), redirect_uri)
    }
}

/// Why a client was refused for its redirect URIs: none is one an admin
/// allows.
const NOT_ALLOWED: &str = "none of the redirect_uris is an app this server allows; an admin \
                           allows apps in the web UI, Settings › MCP, Allowed redirects";

#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    /// What the client sent cannot be a client (`invalid_client_metadata`
    /// or `invalid_redirect_uri`, RFC 7591 §3.2.2).
    #[error("{message}")]
    Invalid { code: &'static str, message: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// An authorization request (`/authorize`'s query).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct AuthorizeRequest {
    pub response_type: Option<String>,
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    pub scope: Option<String>,
    pub state: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
    pub resource: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthorizeError {
    /// Shown on the server's own page; nothing goes to the client.
    #[error("{0}")]
    Broken(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// A request waiting for its user's consent, in memory only.
#[derive(Debug, Clone)]
struct Pending {
    client_id: String,
    client_name: String,
    redirect_uri: String,
    state: Option<String>,
    code_challenge: String,
    /// The scope parameter as sent.
    requested: String,
    /// Unix seconds, by the store's clock.
    at: i64,
}

/// What consent shows of a waiting request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingView {
    /// The name the client registered: its own word.
    pub client_name: String,
    /// Where the code goes (`claude.ai`).
    pub return_host: String,
    /// The scopes the user may grant, `admin` only to an admin.
    pub scopes: Vec<Scope>,
}

#[derive(Debug, thiserror::Error)]
pub enum DecideError {
    /// No such request waiting: decided already, or over.
    #[error("this request is over or was already answered; start again from the app")]
    NoSuchRequest,
    /// Allowed, with nothing ticked, or a scope that was not offered.
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The token endpoint's refusals (RFC 6749 §5.2).
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("unknown client")]
    InvalidClient,
    #[error("the code or refresh token is not valid")]
    InvalidGrant,
    #[error("only authorization_code and refresh_token")]
    UnsupportedGrantType,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl TokenError {
    /// Its `error` code.
    pub fn code(&self) -> &'static str {
        match self {
            TokenError::InvalidRequest(_) => "invalid_request",
            TokenError::InvalidClient => "invalid_client",
            TokenError::InvalidGrant => "invalid_grant",
            TokenError::UnsupportedGrantType => "unsupported_grant_type",
            TokenError::Store(_) => "server_error",
        }
    }
}

/// A token response's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
    /// Space-separated.
    pub scope: String,
}

/// A new access token and refresh token, and their hashes.
struct Pair {
    access: String,
    refresh: String,
    access_hash: String,
    refresh_hash: String,
    /// Unix seconds.
    now: i64,
}

impl Pair {
    fn new(now: i64) -> Self {
        let access = format!("{TOKEN_PREFIX}{}", secret());
        let refresh = format!("{TOKEN_PREFIX}{}", secret());
        Self {
            access_hash: hash(&access),
            refresh_hash: hash(&refresh),
            access,
            refresh,
            now,
        }
    }

    fn stored(&self) -> NewTokens<'_> {
        NewTokens {
            access_hash: &self.access_hash,
            access_expires_at: self.now + ACCESS_TOKEN.as_secs() as i64,
            refresh_hash: &self.refresh_hash,
            refresh_expires_at: self.now + REFRESH_TOKEN.as_secs() as i64,
        }
    }

    fn tokens(self, scope: String) -> Tokens {
        Tokens {
            access_token: self.access,
            refresh_token: self.refresh,
            expires_in: ACCESS_TOKEN.as_secs(),
            scope,
        }
    }
}

/// The authorization server: its store, and the requests waiting for
/// consent.
#[derive(Debug)]
pub struct OAuth {
    store: Store,
    /// The apps allowed to sign in (`mcp.allowed_redirects`), live.
    mcp: McpSwitch,
    pending: parking_lot::Mutex<HashMap<String, Pending>>,
    /// Registrations per address in the current window, and when each
    /// address's window began (Unix seconds). `None`: callers a trusted
    /// proxy did not name, counted together.
    registrations: parking_lot::Mutex<HashMap<Option<std::net::IpAddr>, (u32, i64)>>,
}

impl OAuth {
    pub fn new(store: Store, mcp: McpSwitch) -> Self {
        Self {
            store,
            mcp,
            pending: parking_lot::Mutex::default(),
            registrations: parking_lot::Mutex::default(),
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Whether a caller at `address` may register another client now, and
    /// if so count it: [`MAX_REGISTRATIONS`] an hour per address. Anyone
    /// may register, and each registration is a row kept for a day.
    pub fn may_register(&self, address: Option<std::net::IpAddr>) -> bool {
        let now = self.store.now();
        let window = REGISTRATION_WINDOW.as_secs() as i64;
        let mut counts = self.registrations.lock();
        if counts.len() >= MAX_REGISTRANTS {
            counts.retain(|_, (_, since)| now - *since < window);
        }
        if counts.len() >= MAX_REGISTRANTS && !counts.contains_key(&address) {
            return false;
        }
        let (count, since) = counts.entry(address).or_insert((0, now));
        if now - *since >= window {
            (*count, *since) = (0, now);
        }
        if *count >= MAX_REGISTRATIONS {
            return false;
        }
        *count += 1;
        true
    }

    /// Register a client (RFC 7591): its name (trimmed, at most
    /// [`MAX_CLIENT_NAME`] characters, no control characters; "Unnamed
    /// app" when it gives none) and its redirect URIs, each one
    /// [`check_redirect`] takes and at least one an app allowed in
    /// `mcp.allowed_redirects`. The others are kept, but `/authorize`
    /// takes none that is not allowed.
    pub async fn register(
        &self,
        name: Option<&str>,
        redirect_uris: &[String],
    ) -> Result<ClientRecord, RegisterError> {
        let metadata = |message: String| RegisterError::Invalid {
            code: "invalid_client_metadata",
            message,
        };
        let name = name.map(str::trim).filter(|n| !n.is_empty());
        let name = match name {
            None => "Unnamed app".to_owned(),
            Some(n) if n.chars().count() > MAX_CLIENT_NAME => {
                return Err(metadata(format!(
                    "client_name is longer than {MAX_CLIENT_NAME} characters"
                )));
            }
            Some(n) if n.chars().any(char::is_control) => {
                return Err(metadata("client_name has control characters".into()));
            }
            Some(n) => n.to_owned(),
        };
        if redirect_uris.is_empty() || redirect_uris.len() > MAX_REDIRECT_URIS {
            return Err(RegisterError::Invalid {
                code: "invalid_redirect_uri",
                message: format!("between 1 and {MAX_REDIRECT_URIS} redirect_uris"),
            });
        }
        for uri in redirect_uris {
            check_redirect(uri).map_err(|message| RegisterError::Invalid {
                code: "invalid_redirect_uri",
                message,
            })?;
        }
        if !redirect_uris.iter().any(|u| self.mcp.redirect_allowed(u)) {
            tracing::info!(client = %name, redirect_uris = ?redirect_uris, "an OAuth client was refused: no redirect URI is allowed");
            return Err(RegisterError::Invalid {
                code: "invalid_redirect_uri",
                message: NOT_ALLOWED.into(),
            });
        }
        let id = secret();
        self.store
            .add_oauth_client(&id, &name, redirect_uris)
            .await?;
        tracing::info!(client = %name, redirect_uris = ?redirect_uris, "an OAuth client registered");
        Ok(ClientRecord {
            id,
            name,
            redirect_uris: redirect_uris.to_vec(),
            created_at: self.store.now(),
        })
    }

    /// Check an authorization request and keep it for its user's consent:
    /// its id, which the consent page is opened with. Any fault is
    /// [`AuthorizeError::Broken`], for the server's own page.
    pub async fn authorize(&self, request: &AuthorizeRequest) -> Result<String, AuthorizeError> {
        let broken = |why: &str| Err(AuthorizeError::Broken(why.to_owned()));
        let Some(client_id) = request.client_id.as_deref() else {
            return broken("The app did not say who it is (no client_id).");
        };
        let Some(client) = self.store.oauth_client(client_id).await? else {
            return broken("The app is not registered here, or its registration lapsed.");
        };
        let Some(redirect_uri) = request.redirect_uri.as_deref() else {
            return broken("The app did not say where to return (no redirect_uri).");
        };
        if !client
            .redirect_uris
            .iter()
            .any(|r| redirect_matches(r, redirect_uri))
        {
            return broken("The app asked to return to an address it did not register.");
        }
        // Checked again, as an admin may have removed the app since it
        // registered: it signs no one in from then on.
        if !self.mcp.redirect_allowed(redirect_uri) {
            return broken(
                "This server no longer allows this app to sign in. An admin allows apps in \
                 Settings › MCP.",
            );
        }
        if request.response_type.as_deref() != Some("code") {
            return broken("The app asked for something other than a code (response_type).");
        }
        let Some(challenge) = request.code_challenge.as_deref() else {
            return broken("The app sent no PKCE challenge, which is required.");
        };
        if request.code_challenge_method.as_deref() != Some("S256") || !valid_challenge(challenge) {
            return broken("The app's PKCE challenge is not S256.");
        }
        let now = self.store.now();
        let mut pending = self.pending.lock();
        pending.retain(|_, p| now - p.at < PENDING.as_secs() as i64);
        if pending.len() >= MAX_PENDING {
            return broken("Too many sign-ins are waiting; try again in a few minutes.");
        }
        let id = secret();
        pending.insert(
            id.clone(),
            Pending {
                client_id: client.id,
                client_name: client.name,
                redirect_uri: redirect_uri.to_owned(),
                state: request.state.clone(),
                code_challenge: challenge.to_owned(),
                requested: request.scope.clone().unwrap_or_default(),
                at: now,
            },
        );
        Ok(id)
    }

    /// The waiting request `id`, as consent shows it to a user who is an
    /// admin or not.
    pub fn pending(&self, id: &str, admin: bool) -> Option<PendingView> {
        let now = self.store.now();
        let pending = self.pending.lock();
        let p = pending
            .get(id)
            .filter(|p| now - p.at < PENDING.as_secs() as i64)?;
        Some(PendingView {
            client_name: p.client_name.clone(),
            return_host: return_host(&p.redirect_uri),
            scopes: offered(&p.requested, admin),
        })
    }

    /// `user`'s answer to the waiting request `id`: where to send their
    /// browser, with a code for `chosen` (some of the scopes offered), or
    /// with `access_denied`. Answered once.
    pub async fn decide(
        &self,
        id: &str,
        user: LocalUserId,
        admin: bool,
        allow: bool,
        chosen: &[String],
    ) -> Result<url::Url, DecideError> {
        let now = self.store.now();
        let request = {
            let pending = self.pending.lock();
            pending
                .get(id)
                .filter(|p| now - p.at < PENDING.as_secs() as i64)
                .cloned()
                .ok_or(DecideError::NoSuchRequest)?
        };
        let granted = if allow {
            let offered = offered(&request.requested, admin);
            let mut granted = Vec::new();
            for name in chosen {
                match Scope::parse(name).filter(|s| offered.contains(s)) {
                    Some(scope) => granted.push(scope),
                    None => {
                        return Err(DecideError::Invalid(format!(
                            "{name:?} was not offered to this app"
                        )));
                    }
                }
            }
            granted.sort();
            granted.dedup();
            if granted.is_empty() {
                return Err(DecideError::Invalid(
                    "tick at least one thing to allow, or deny".into(),
                ));
            }
            Some(granted)
        } else {
            None
        };
        // Answered once: taken now, whatever comes of it.
        if self.pending.lock().remove(id).is_none() {
            return Err(DecideError::NoSuchRequest);
        }
        // An app removed from the list while its user was on the consent
        // page gets nothing, not even a denial: never a redirect to an
        // address the server does not allow.
        if !self.mcp.redirect_allowed(&request.redirect_uri) {
            return Err(DecideError::Invalid(
                "this server no longer allows this app to sign in".into(),
            ));
        }
        let mut to = url::Url::parse(&request.redirect_uri)
            .map_err(|e| DecideError::Invalid(e.to_string()))?;
        let Some(granted) = granted else {
            tracing::info!(client = %request.client_name, user = %user, "an OAuth client was denied");
            to.query_pairs_mut().append_pair("error", "access_denied");
            if let Some(state) = &request.state {
                to.query_pairs_mut().append_pair("state", state);
            }
            return Ok(to);
        };
        let code = secret();
        let scopes = scope_string(&granted);
        self.store
            .grant_oauth(NewGrant {
                client_id: &request.client_id,
                user,
                scopes: &scopes,
                redirect_uri: &request.redirect_uri,
                code_hash: &hash(&code),
                code_challenge: &request.code_challenge,
                code_expires_at: now + CODE.as_secs() as i64,
            })
            .await?;
        tracing::info!(client = %request.client_name, user = %user, scopes, "an OAuth client was granted access");
        to.query_pairs_mut().append_pair("code", &code);
        if let Some(state) = &request.state {
            to.query_pairs_mut().append_pair("state", state);
        }
        Ok(to)
    }

    /// Trade a code for tokens (`grant_type=authorization_code`): the
    /// client that asked for it, the redirect URI it named, and the PKCE
    /// verifier its challenge was made from.
    pub async fn exchange(
        &self,
        client_id: &str,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<Tokens, TokenError> {
        if self.store.oauth_client(client_id).await?.is_none() {
            return Err(TokenError::InvalidClient);
        }
        if !valid_verifier(verifier) {
            return Err(TokenError::InvalidRequest(
                "code_verifier is not a PKCE verifier".into(),
            ));
        }
        let (grant_id, scopes) = match self.store.redeem_oauth_code(&hash(code)).await? {
            Redeemed::Fresh {
                grant_id,
                client_id: owner,
                redirect_uri: named,
                code_challenge,
                scopes,
            } if owner == client_id
                && named == redirect_uri
                && pkce_matches(verifier, &code_challenge) =>
            {
                (grant_id, scopes)
            }
            Redeemed::Reused => {
                tracing::warn!(
                    client_id,
                    "an OAuth code was used twice: its grant is revoked"
                );
                return Err(TokenError::InvalidGrant);
            }
            _ => return Err(TokenError::InvalidGrant),
        };
        let pair = Pair::new(self.store.now());
        self.store
            .add_oauth_tokens(&grant_id, pair.stored())
            .await?;
        Ok(pair.tokens(scopes))
    }

    /// Trade a refresh token for a new pair (`grant_type=refresh_token`):
    /// the old one is spent, in the same transaction.
    pub async fn refresh(&self, client_id: &str, refresh: &str) -> Result<Tokens, TokenError> {
        if self.store.oauth_client(client_id).await?.is_none() {
            return Err(TokenError::InvalidClient);
        }
        let pair = Pair::new(self.store.now());
        match self
            .store
            .refresh_oauth(&hash(refresh), client_id, pair.stored())
            .await?
        {
            Refreshed::Rotated { scopes } => Ok(pair.tokens(scopes)),
            Refreshed::Reused => {
                tracing::warn!(
                    client_id,
                    "an OAuth refresh token was used twice: its grant is revoked"
                );
                Err(TokenError::InvalidGrant)
            }
            Refreshed::Unknown => Err(TokenError::InvalidGrant),
        }
    }

    /// Whose the access token `token` is, if it is live.
    pub async fn access(&self, token: &str) -> Result<Option<OauthAccess>, StoreError> {
        if !token.starts_with(TOKEN_PREFIX) {
            return Ok(None);
        }
        self.store.oauth_access(&hash(token)).await
    }
}

#[cfg(test)]
mod tests;
