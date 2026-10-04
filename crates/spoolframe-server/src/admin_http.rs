//! The admin listener's HTTP side (P3-01): the browser's way in.
//!
//! A browser cannot put an `Authorization` header on a WebSocket upgrade,
//! and a JSON-RPC `me.login` over the socket cannot set a cookie, which
//! only an HTTP response can. So signing in from the web UI is plain HTTP
//! on the same listener (`/auth/*`), and its session travels as an
//! `HttpOnly` cookie that the JSON-RPC socket accepts as it does the CLI's
//! Bearer token. Pages, `/auth` and the socket share one origin: no CORS.
//!
//! The `/auth` handlers call the same methods the socket serves, so a
//! login, a setup or a logout has one code path, one rate limit and one
//! set of errors whichever way it comes.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Router};
use jsonrpsee::Extensions;
use jsonrpsee::server::{Methods, ServerHandle, StopHandle, stop_channel};
use jsonrpsee::types::ErrorObjectOwned;
use serde_json::json;
use spoolframe_admin_api::{
    AdminApiServer, Credentials, LoggedIn, MeApiServer, PasskeySetupBegin, PasskeySetupFinish,
    SetupRequest, error_code,
};

use crate::admin::{AdminApi, Bearer, Peer};
use crate::auth::API_SESSION;

/// The browser's session cookie: an admin API session token, as the CLI's
/// Bearer token is.
pub const COOKIE: &str = "spoolframe_session";

/// Where a browser is allowed to come from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Origins {
    /// Only from the page's own origin: `Origin` names the host the
    /// request was sent to. What a loopback listener reached directly or
    /// over an SSH tunnel needs, whatever port the tunnel uses.
    #[default]
    SameHost,
    /// Only from `admin.public_url` (`https://media.example.org`), behind a
    /// TLS proxy, which may rewrite `Host`.
    Exactly(String),
}

impl Origins {
    /// The web UI's public origin, if it has one: where a link to one of
    /// its pages can point from another device.
    pub fn public_url(&self) -> Option<&str> {
        match self {
            Origins::SameHost => None,
            Origins::Exactly(origin) => Some(origin),
        }
    }

    /// Whether a request with these headers may act. Without `Origin` it
    /// is not a browser acting for another site (browsers send it on every
    /// POST and WebSocket upgrade): the CLI, curl.
    fn allow(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
            return true;
        };
        match self {
            Origins::Exactly(allowed) => origin == allowed,
            Origins::SameHost => {
                let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
                let origin_host = origin
                    .strip_prefix("http://")
                    .or_else(|| origin.strip_prefix("https://"));
                host.is_some() && origin_host == host
            }
        }
    }
}

/// The reverse proxies in front of the admin listener
/// (`admin.trusted_proxies`): addresses and CIDR ranges. Only a connection
/// from one of them is believed about who it carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedProxies(Vec<Network>);

/// An address range: `addr/prefix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Network {
    addr: IpAddr,
    prefix: u32,
}

impl Network {
    fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - self.prefix).unwrap_or(0);
                (u32::from(net) ^ u32::from(ip)) & mask == 0
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - self.prefix).unwrap_or(0);
                (u128::from(net) ^ u128::from(ip)) & mask == 0
            }
            _ => false,
        }
    }
}

impl std::str::FromStr for Network {
    type Err = String;

    /// `10.0.0.5`, `172.17.0.0/16`, `fd00::/8`.
    fn from_str(text: &str) -> Result<Self, String> {
        let (addr, prefix) = match text.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (text, None),
        };
        let addr: IpAddr = addr
            .parse()
            .map_err(|_| format!("{text:?} is not an address or a CIDR range"))?;
        let addr = addr.to_canonical();
        let bits = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => bits,
            Some(prefix) => prefix
                .parse()
                .ok()
                .filter(|p| *p <= bits)
                .ok_or_else(|| format!("{text:?}: the prefix is not a number up to {bits}"))?,
        };
        Ok(Self { addr, prefix })
    }
}

impl std::str::FromStr for TrustedProxies {
    type Err = String;

    /// A comma-separated list of addresses and CIDR ranges.
    fn from_str(list: &str) -> Result<Self, String> {
        list.split(',')
            .map(|entry| entry.trim().parse())
            .collect::<Result<_, _>>()
            .map(Self)
    }
}

/// The header a trusted proxy names its caller in. `Forwarded` (RFC 7239)
/// is not read.
const FORWARDED_FOR: &str = "x-forwarded-for";

impl TrustedProxies {
    fn contains(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|network| network.contains(ip))
    }

    /// The address the caller of a request from `peer` is at, for the
    /// failure limit. From anyone but a trusted proxy that is `peer`, and
    /// the header is not read: anyone can send one. From a trusted proxy
    /// it is the right-most `X-Forwarded-For` address that is not a
    /// trusted proxy itself: each proxy appends the address it was called
    /// from, so that is the first one a proxy of ours did not write, and
    /// what is left of it is the caller's own claim. `None` when a trusted
    /// proxy sent no such address: then nothing says who the caller is,
    /// and the proxy's own address would name everyone behind it.
    pub(crate) fn caller(&self, peer: IpAddr, headers: &HeaderMap) -> Option<IpAddr> {
        if !self.contains(peer) {
            return Some(peer);
        }
        let hops: Vec<Option<IpAddr>> = headers
            .get_all(FORWARDED_FOR)
            .iter()
            .flat_map(|value| match value.to_str() {
                Ok(list) => list.split(',').map(|hop| hop.trim().parse().ok()).collect(),
                Err(_) => vec![None],
            })
            .collect();
        for hop in hops.into_iter().rev() {
            match hop {
                Some(ip) if self.contains(ip) => {}
                // An entry that is not an address is not one a proxy of
                // ours wrote; nothing left of it can be believed.
                other => return other,
            }
        }
        None
    }
}

/// Serve `methods`, the `/auth` endpoints and the web UI's pages on
/// `listen`.
pub(crate) async fn serve(
    listen: SocketAddr,
    api: AdminApi,
    methods: Methods,
    origins: Origins,
    proxies: TrustedProxies,
) -> std::io::Result<(SocketAddr, ServerHandle)> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let addr = listener.local_addr()?;
    let (stop, handle) = stop_channel();
    let app = router(api, methods, stop.clone(), origins, proxies);
    tokio::spawn(async move {
        let shutdown = stop.shutdown();
        // With each connection's address, which the failure limit counts.
        let app = app.into_make_service_with_connect_info::<SocketAddr>();
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await
        {
            tracing::error!(error = %e, "the admin listener failed");
        }
    });
    Ok((addr, handle))
}

fn router(
    api: AdminApi,
    methods: Methods,
    stop: StopHandle,
    origins: Origins,
    proxies: TrustedProxies,
) -> Router {
    let rpc = tower::service_fn(move |req: Request| {
        // Built per connection: building is what gives each its own id.
        let mut service = jsonrpsee::server::ServerBuilder::default()
            .to_service_builder()
            .build(methods.clone(), stop.clone());
        async move {
            let response = match tower::Service::call(&mut service, req).await {
                Ok(response) => response.map(Body::new),
                Err(e) => {
                    tracing::warn!(error = %e, "admin JSON-RPC service");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            };
            Ok::<_, Infallible>(response)
        }
    });
    let web = crate::web::Web::default();
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/passkey/begin", post(passkey_begin))
        .route("/auth/passkey/finish", post(passkey_finish))
        .route("/auth/setup", get(setup_open).post(setup))
        .route("/auth/setup/passkey/begin", post(setup_passkey_begin))
        .route("/auth/setup/passkey/finish", post(setup_passkey_finish))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .route("/auth/site", get(site))
        .with_state(api)
        // The socket: `/jsonrpc` for the web UI, and any other path's
        // upgrade too, which is where the CLI has always connected. Except
        // dx serve's own in a debug build that takes its pages from it.
        .route_service("/jsonrpc", rpc.clone())
        .fallback_service(tower::service_fn(move |req: Request| {
            let mut rpc = rpc.clone();
            let web = web.clone();
            async move {
                if is_upgrade(req.headers()) && !web.takes_upgrade(req.uri().path()) {
                    return tower::Service::call(&mut rpc, req).await;
                }
                Ok::<_, Infallible>(web.page(req).await)
            }
        }))
        .layer(middleware::from_fn_with_state(proxies, credentials))
        .layer(middleware::from_fn_with_state(origins, check_origin))
}

fn is_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Refuse what another site's page could make a signed-in browser do: a
/// POST or a WebSocket upgrade from a foreign `Origin`. `SameSite=Strict`
/// already keeps the cookie off them; this does not rely on it.
async fn check_origin(State(origins): State<Origins>, req: Request, next: Next) -> Response<Body> {
    let acts = req.method() == axum::http::Method::POST || is_upgrade(req.headers());
    if acts && !origins.allow(req.headers()) {
        tracing::warn!(origin = ?req.headers().get(header::ORIGIN), "admin request from another origin refused");
        return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
    }
    next.run(req).await
}

/// The request's session token where the methods find it: the Bearer
/// header (the CLI) or else the cookie (a browser). A wrong Bearer token
/// is not rescued by a cookie. And the address its caller is at.
async fn credentials(
    State(proxies): State<TrustedProxies>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut req: Request,
    next: Next,
) -> Response<Body> {
    let caller = proxies.caller(peer.ip(), req.headers());
    if caller.is_none() {
        tracing::warn!(
            proxy = %peer.ip(),
            "a trusted proxy sent no usable X-Forwarded-For; the failure limit cannot count its callers by address"
        );
    }
    req.extensions_mut().insert(Peer(caller));
    if let Some(token) = token(req.headers()) {
        req.extensions_mut().insert(Bearer(token));
    }
    next.run(req).await
}

fn token(headers: &HeaderMap) -> Option<String> {
    if let Some(bearer) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        return Some(bearer.trim().to_owned());
    }
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)
        .map(|(_, value)| value.to_owned())
}

/// `Secure` always: browsers treat `http://localhost` as secure, so the
/// cookie works on loopback, over an SSH tunnel to it, and behind a TLS
/// proxy alike.
fn set_cookie(token: &str) -> HeaderValue {
    let value = format!(
        "{COOKIE}={token}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age={}",
        API_SESSION.as_secs()
    );
    HeaderValue::from_str(&value).expect("tokens are hex")
}

fn clear_cookie() -> HeaderValue {
    HeaderValue::from_static(
        "spoolframe_session=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0",
    )
}

/// A JSON-RPC error as an HTTP answer, with the same code and message, so
/// the web UI reads both alike.
fn failure(e: ErrorObjectOwned) -> Response<Body> {
    let status = match e.code() {
        error_code::UNAUTHENTICATED
        | error_code::INVALID_CREDENTIALS
        | error_code::PASSKEY_REFUSED => StatusCode::UNAUTHORIZED,
        error_code::FORBIDDEN | error_code::INVALID_SETUP_CODE => StatusCode::FORBIDDEN,
        error_code::RATE_LIMITED => StatusCode::TOO_MANY_REQUESTS,
        error_code::ALREADY_SET_UP | error_code::USER_EXISTS => StatusCode::CONFLICT,
        error_code::INVALID_INPUT => StatusCode::BAD_REQUEST,
        error_code::NOT_CONFIGURED => StatusCode::NOT_IMPLEMENTED,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let body = json!({ "code": e.code(), "message": e.message(), "data": e.data() });
    (status, axum::Json(body)).into_response()
}

/// Signed in: the cookie, and who. From a page the browser will not keep
/// the cookie for, the sign-in looks to its user as if nothing happened,
/// and only this log can say why.
fn signed_in(headers: &HeaderMap, done: LoggedIn) -> Response<Body> {
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        && drops_secure_cookies(origin)
    {
        tracing::warn!(
            username = %done.username,
            origin,
            "signed in from a plain HTTP page: the browser will drop the session cookie, so the \
             sign-in will appear to do nothing; open the web UI over HTTPS (admin.public_url) or \
             at http://localhost"
        );
    }
    let mut response =
        axum::Json(json!({ "username": done.username, "role": done.role })).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, set_cookie(&done.token));
    response
}

/// Whether a page at `origin` is one a browser keeps no `Secure` cookie
/// for: plain HTTP on anything but loopback. `Origin` is the page's own
/// scheme and host as the browser sees them, which `Host` and the
/// listener's plain HTTP are not behind a TLS proxy.
fn drops_secure_cookies(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    if url.scheme() != "http" {
        return false;
    }
    match url.host() {
        Some(url::Host::Domain(name)) => name != "localhost" && !name.ends_with(".localhost"),
        Some(url::Host::Ipv4(ip)) => !ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => !ip.is_loopback(),
        None => false,
    }
}

/// The JSON body as `T`, or why not (answered as a 400).
fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ErrorObjectOwned> {
    serde_json::from_slice(body)
        .map_err(|e| ErrorObjectOwned::owned::<()>(error_code::INVALID_INPUT, e.to_string(), None))
}

async fn login(
    State(api): State<AdminApi>,
    Extension(peer): Extension<Peer>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    let credentials: Credentials = match parse(&body) {
        Ok(c) => c,
        Err(e) => return failure(e),
    };
    match MeApiServer::login(&api, &from(peer), credentials).await {
        Ok(done) => signed_in(&headers, done),
        Err(e) => failure(e),
    }
}

/// Begin a passkey sign-in (P3-05): the ceremony and the options for the
/// browser's `navigator.credentials.get`.
async fn passkey_begin(
    State(api): State<AdminApi>,
    Extension(peer): Extension<Peer>,
) -> Response<Body> {
    match MeApiServer::passkey_login_begin(&api, &from(peer)).await {
        Ok(challenge) => axum::Json(challenge).into_response(),
        Err(e) => failure(e),
    }
}

/// What finishes a passkey sign-in.
#[derive(serde::Deserialize)]
struct PasskeyAssertion {
    ceremony: String,
    credential: serde_json::Value,
}

/// Finish it: signed in as a password signs in, with the same cookie.
async fn passkey_finish(
    State(api): State<AdminApi>,
    Extension(peer): Extension<Peer>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    let PasskeyAssertion {
        ceremony,
        credential,
    } = match parse(&body) {
        Ok(assertion) => assertion,
        Err(e) => return failure(e),
    };
    match MeApiServer::passkey_login_finish(&api, &from(peer), ceremony, credential).await {
        Ok(done) => signed_in(&headers, done),
        Err(e) => failure(e),
    }
}

async fn setup(
    State(api): State<AdminApi>,
    Extension(peer): Extension<Peer>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    let request: SetupRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return failure(e),
    };
    match AdminApiServer::setup(&api, &from(peer), request).await {
        Ok(done) => signed_in(&headers, done),
        Err(e) => failure(e),
    }
}

/// Begin creating the first admin with a passkey (P3-01): the ceremony and
/// the options for the browser's `navigator.credentials.create`.
async fn setup_passkey_begin(
    State(api): State<AdminApi>,
    Extension(peer): Extension<Peer>,
    body: axum::body::Bytes,
) -> Response<Body> {
    let request: PasskeySetupBegin = match parse(&body) {
        Ok(r) => r,
        Err(e) => return failure(e),
    };
    match AdminApiServer::setup_passkey_begin(&api, &from(peer), request).await {
        Ok(challenge) => axum::Json(challenge).into_response(),
        Err(e) => failure(e),
    }
}

/// Finish it: the admin exists, signed in as `setup` signs them in.
async fn setup_passkey_finish(
    State(api): State<AdminApi>,
    Extension(peer): Extension<Peer>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    let request: PasskeySetupFinish = match parse(&body) {
        Ok(r) => r,
        Err(e) => return failure(e),
    };
    match AdminApiServer::setup_passkey_finish(&api, &from(peer), request).await {
        Ok(done) => signed_in(&headers, done),
        Err(e) => failure(e),
    }
}

/// `{"open": true}` while the first admin can still be set up: a signed-out
/// browser shows setup rather than login. `passkeysUnavailable` says why
/// the server cannot make the admin a passkey, if it cannot: then the page
/// offers a password only.
async fn setup_open(State(api): State<AdminApi>) -> Response<Body> {
    match api.setup_open().await {
        Ok(open) => axum::Json(json!({
            "open": open,
            "passkeysUnavailable": api.passkeys_unavailable(),
        }))
        .into_response(),
        Err(e) => failure(e),
    }
}

/// The server's name and public URL, for a signed-out page.
async fn site(State(api): State<AdminApi>) -> Response<Body> {
    axum::Json(api.site()).into_response()
}

/// What a method called before there is a session gets, as the socket
/// would give it: where the caller is.
fn from(peer: Peer) -> Extensions {
    let mut ext = Extensions::new();
    ext.insert(peer);
    ext
}

/// The session's methods, called as the socket would call them.
fn as_caller(req: &Request) -> Extensions {
    let mut ext = Extensions::new();
    if let Some(bearer) = req.extensions().get::<Bearer>() {
        ext.insert(bearer.clone());
    }
    ext
}

/// End the session and clear the cookie. Without a live session there is
/// nothing to end, and the cookie is cleared all the same.
async fn logout(State(api): State<AdminApi>, req: Request) -> Response<Body> {
    let ext = as_caller(&req);
    if let Err(e) = MeApiServer::logout(&api, &ext).await
        && e.code() != error_code::UNAUTHENTICATED
    {
        return failure(e);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, clear_cookie());
    response
}

/// Who the cookie signs in, read now (role included): the web UI asks
/// before opening the socket, whose refusal a browser cannot read.
async fn me(State(api): State<AdminApi>, req: Request) -> Response<Body> {
    match MeApiServer::who_am_i(&api, &as_caller(&req)).await {
        Ok(user) => axum::Json(user).into_response(),
        Err(e) => failure(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(name.clone(), HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn same_host_takes_the_page_s_own_origin_only() {
        let origins = Origins::SameHost;
        let own = headers(&[
            (header::HOST, "localhost:7421"),
            (header::ORIGIN, "http://localhost:7421"),
        ]);
        assert!(origins.allow(&own));
        let other = headers(&[
            (header::HOST, "localhost:7421"),
            (header::ORIGIN, "https://evil.example"),
        ]);
        assert!(!origins.allow(&other));
        // Another port on the same host is another origin.
        let port = headers(&[
            (header::HOST, "localhost:7421"),
            (header::ORIGIN, "http://localhost:8080"),
        ]);
        assert!(!origins.allow(&port));
        // Not a browser.
        assert!(origins.allow(&headers(&[(header::HOST, "localhost:7421")])));
    }

    #[test]
    fn a_public_url_is_the_only_origin_behind_a_proxy() {
        let origins = Origins::Exactly("https://media.example.org".into());
        assert!(origins.allow(&headers(&[
            (header::HOST, "127.0.0.1:7421"),
            (header::ORIGIN, "https://media.example.org"),
        ])));
        assert!(!origins.allow(&headers(&[
            (header::HOST, "127.0.0.1:7421"),
            (header::ORIGIN, "http://127.0.0.1:7421"),
        ])));
    }

    fn proxies(list: &str) -> TrustedProxies {
        list.parse().unwrap()
    }

    fn forwarded(values: &[&str]) -> HeaderMap {
        let pairs: Vec<_> = values
            .iter()
            .map(|v| (header::HeaderName::from_static(FORWARDED_FOR), *v))
            .collect();
        headers(&pairs)
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn a_trusted_proxy_s_caller_is_the_address_it_forwards_for() {
        let proxies = proxies("10.0.0.5, 172.17.0.0/16");
        let from = forwarded(&["203.0.113.9"]);
        assert_eq!(
            proxies.caller(ip("10.0.0.5"), &from),
            Some(ip("203.0.113.9"))
        );
        assert_eq!(
            proxies.caller(ip("172.17.0.1"), &from),
            Some(ip("203.0.113.9"))
        );
        assert_eq!(
            proxies.caller(ip("::ffff:10.0.0.5"), &from),
            Some(ip("203.0.113.9"))
        );
    }

    #[test]
    fn a_forwarding_header_from_anyone_else_is_not_read() {
        let from = forwarded(&["203.0.113.9"]);
        let proxies = proxies("10.0.0.5, 172.17.0.0/16");
        assert_eq!(proxies.caller(ip("10.0.0.6"), &from), Some(ip("10.0.0.6")));
        assert_eq!(
            proxies.caller(ip("172.18.0.1"), &from),
            Some(ip("172.18.0.1"))
        );
        // With no proxies set, the connection's address, loopback too.
        let none = TrustedProxies::default();
        assert_eq!(none.caller(ip("127.0.0.1"), &from), Some(ip("127.0.0.1")));
        assert_eq!(none.caller(ip("10.0.0.5"), &from), Some(ip("10.0.0.5")));
    }

    #[test]
    fn the_caller_is_the_right_most_address_no_trusted_proxy_wrote() {
        let proxies = proxies("10.0.0.0/24");
        // The caller sent a header of its own; two of our proxies in a row.
        let chain = forwarded(&["198.51.100.1, 203.0.113.9, 10.0.0.6"]);
        assert_eq!(
            proxies.caller(ip("10.0.0.5"), &chain),
            Some(ip("203.0.113.9"))
        );
        // The same over two header lines.
        let lines = forwarded(&["198.51.100.1", "203.0.113.9, 10.0.0.6"]);
        assert_eq!(
            proxies.caller(ip("10.0.0.5"), &lines),
            Some(ip("203.0.113.9"))
        );
    }

    #[test]
    fn a_trusted_proxy_without_a_usable_header_names_no_caller() {
        let proxies = proxies("10.0.0.0/24");
        assert_eq!(proxies.caller(ip("10.0.0.5"), &HeaderMap::new()), None);
        // Only our own proxies.
        assert_eq!(
            proxies.caller(ip("10.0.0.5"), &forwarded(&["10.0.0.6"])),
            None
        );
        // Not an address where the caller's should be.
        assert_eq!(
            proxies.caller(ip("10.0.0.5"), &forwarded(&["203.0.113.9, unknown"])),
            None
        );
    }

    #[test]
    fn trusted_proxies_are_addresses_and_ranges() {
        let proxies = proxies("10.0.0.5,fd00::/8, 0.0.0.0/0");
        assert!(proxies.contains(ip("10.0.0.5")));
        assert!(proxies.contains(ip("fd12::1")));
        assert!(proxies.contains(ip("192.0.2.1")));
        assert!(!proxies.contains(ip("2001:db8::1")));
        for bad in [
            "",
            "10.0.0.5,",
            "proxy.example.org",
            "10.0.0.0/33",
            "fd00::/x",
        ] {
            assert!(bad.parse::<TrustedProxies>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_bearer_header_wins_over_the_cookie() {
        let both = headers(&[
            (header::AUTHORIZATION, "Bearer cli"),
            (header::COOKIE, "a=b; spoolframe_session=web"),
        ]);
        assert_eq!(token(&both).as_deref(), Some("cli"));
        let cookie = headers(&[(header::COOKIE, "a=b; spoolframe_session=web; c=d")]);
        assert_eq!(token(&cookie).as_deref(), Some("web"));
        assert_eq!(token(&headers(&[(header::COOKIE, "a=b")])), None);
    }

    #[test]
    fn the_cookie_is_http_only_secure_and_strict() {
        let cookie = set_cookie("abc");
        let cookie = cookie.to_str().unwrap();
        for part in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/"] {
            assert!(cookie.contains(part), "{cookie}");
        }
    }
}
