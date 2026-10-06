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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Router};
use jsonrpsee::Extensions;
use jsonrpsee::server::middleware::rpc::{Batch, Notification, RpcServiceBuilder, RpcServiceT};
use jsonrpsee::server::{Methods, ServerHandle, StopHandle, stop_channel};
use jsonrpsee::types::ErrorObjectOwned;
use serde_json::json;
use spoolframe_admin_api::{
    AdminApiServer, Credentials, LoggedIn, MeApiServer, PasskeySetupBegin, PasskeySetupFinish,
    SetupRequest, error_code,
};

use crate::admin::{AdminApi, Bearer, PageOrigin, Peer, SsoOutcome};
use crate::auth::API_SESSION;

/// The browser's session cookie: an admin API session token, as the CLI's
/// Bearer token is.
pub const COOKIE: &str = "spoolframe_session";

/// A sign-in provider flow's cookie (P3-04): binds the flow to the browser
/// that began it. `SameSite=Lax`, unlike the session's: the provider sends
/// the browser back with a top-level navigation from its own site, which
/// carries only a Lax cookie. It names no one and opens nothing by itself.
pub const OIDC_COOKIE: &str = "spoolframe_oidc";

/// How the session cookie is sent (`admin.insecure_cookies`, P3-10): a
/// live setting, so the running listener reads it at every sign-in.
#[derive(Debug, Clone, Default)]
pub struct CookiePolicy(Arc<AtomicBool>);

impl CookiePolicy {
    pub fn new(insecure: bool) -> Self {
        Self(Arc::new(AtomicBool::new(insecure)))
    }

    /// Whether the cookie goes without `Secure`.
    pub fn insecure(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn set_insecure(&self, insecure: bool) {
        self.0.store(insecure, Ordering::Relaxed);
    }

    /// `Set-Cookie` for a new session. `HttpOnly` always, and `Secure`
    /// unless insecure cookies are on: browsers treat `http://localhost`
    /// as secure, so a `Secure` cookie works on loopback, over an SSH
    /// tunnel to it, and behind a TLS proxy alike, but on no plain-HTTP
    /// LAN address. Insecure, it does there too; page scripts still cannot
    /// read it (PX-11, PX-09 finding 21).
    fn set(&self, token: &str) -> HeaderValue {
        let value = format!(
            "{COOKIE}={token}; Path=/; {}SameSite=Strict; Max-Age={}",
            self.flags(),
            API_SESSION.as_secs()
        );
        HeaderValue::from_str(&value).expect("tokens are hex")
    }

    fn clear(&self) -> HeaderValue {
        HeaderValue::from_str(&format!(
            "{COOKIE}=; Path=/; {}SameSite=Strict; Max-Age=0",
            self.flags()
        ))
        .expect("a fixed cookie")
    }

    /// `Set-Cookie` for a sign-in provider flow begun in this browser:
    /// sent back only to `/auth/oidc`, and for as long as the flow lives.
    fn oidc(&self, value: &str) -> HeaderValue {
        HeaderValue::from_str(&format!(
            "{OIDC_COOKIE}={value}; Path=/auth/oidc; {}SameSite=Lax; Max-Age={}",
            self.flags(),
            crate::oidc::FLOW_LIFETIME.as_secs()
        ))
        .expect("the value is hex")
    }

    fn oidc_clear(&self) -> HeaderValue {
        HeaderValue::from_str(&format!(
            "{OIDC_COOKIE}=; Path=/auth/oidc; {}SameSite=Lax; Max-Age=0",
            self.flags()
        ))
        .expect("a fixed cookie")
    }

    fn flags(&self) -> &'static str {
        if self.insecure() {
            "HttpOnly; "
        } else {
            "HttpOnly; Secure; "
        }
    }
}

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
pub struct Network {
    addr: IpAddr,
    prefix: u32,
}

impl Network {
    pub fn contains(&self, ip: IpAddr) -> bool {
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

/// Addresses and CIDR ranges a scraper may read `GET /metrics` from
/// (`admin.metrics_from`, P3-14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressRanges(Vec<Network>);

impl AddressRanges {
    /// What `admin.metrics_from` is when left out.
    pub const LOOPBACK: &'static str = "127.0.0.0/8, ::1";

    /// This machine's loopback addresses, IPv4 and IPv6.
    pub fn loopback() -> Self {
        Self::LOOPBACK.parse().expect("fixed ranges")
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|network| network.contains(ip))
    }
}

impl std::str::FromStr for AddressRanges {
    type Err = String;

    /// A comma-separated list of addresses and CIDR ranges; an empty one
    /// lets nobody in.
    fn from_str(list: &str) -> Result<Self, String> {
        list.split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::parse)
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
    let metrics = api.metrics_arc();
    let rpc = tower::service_fn(move |req: Request| {
        // Each socket counted open while its RPC service lives (P3-14):
        // jsonrpsee makes one per connection and drops it as the
        // connection ends.
        let metrics = metrics.clone();
        let counted = RpcServiceBuilder::new().layer_fn(move |inner| Counted {
            inner,
            _open: metrics.as_ref().map(|m| Arc::new(m.web_connection())),
        });
        // Built per connection: building is what gives each its own id.
        // No client sends batches; one would carry any number of calls,
        // password checks among them, in one message (PX-10, finding 10).
        let mut service = jsonrpsee::server::ServerBuilder::default()
            .set_config(
                jsonrpsee::server::ServerConfig::builder()
                    .set_batch_request_config(jsonrpsee::server::BatchRequestConfig::Disabled)
                    .build(),
            )
            .set_rpc_middleware(counted)
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
        .route("/auth/setup/check", post(setup_check))
        .route("/auth/setup/passkey/begin", post(setup_passkey_begin))
        .route("/auth/setup/passkey/finish", post(setup_passkey_finish))
        .route("/auth/oidc/begin", post(sso_begin))
        .route("/auth/oidc/callback", get(sso_callback))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .route("/auth/site", get(site))
        .route("/catalog/artwork", get(catalog_artwork))
        .route("/metrics", get(metrics_page))
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
        // Outermost: refusals get the headers too.
        .layer(middleware::from_fn(security_headers))
}

/// The web UI's content policy (PX-11 finding 37), fitted to the Dioxus
/// bundle: its own scripts, and `wasm-unsafe-eval` to compile its wasm
/// (no `unsafe-eval`: the app never evaluates script). Styles allow
/// `unsafe-inline`, as the app sets `style` attributes and puts its font
/// faces in a `<style>` element. Images from the listener (artwork) and
/// data or blob URLs; the socket is `'self'`. Never framed.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
    script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; \
    object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// A page `dx serve` made, in a debug build that takes its pages from it:
/// its hot reload is not fitted to [`CONTENT_SECURITY_POLICY`], which is
/// left off it. A release build never has one.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DevPage;

/// Every answer (PX-11 finding 37): not to be sniffed as another type,
/// nor framed, with the web UI's content policy. Sign-in answers and
/// JSON-RPC replies are not to be stored: they name sessions and users.
async fn security_headers(req: Request, next: Next) -> Response<Body> {
    let path = req.uri().path().to_owned();
    let mut response = next.run(req).await;
    let dev_page = response.extensions().get::<DevPage>().is_some();
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    if !dev_page {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CONTENT_SECURITY_POLICY),
        );
    }
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|t| t.to_str().ok())
        .is_some_and(|t| t.starts_with("application/json"));
    if path.starts_with("/auth/") || path == "/jsonrpc" || json {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

/// An admin API connection's RPC service, holding its place in the count
/// of open sockets until jsonrpsee drops it.
#[derive(Clone)]
struct Counted<S> {
    inner: S,
    _open: Option<Arc<crate::metrics::WebConnection>>,
}

impl<S> RpcServiceT for Counted<S>
where
    S: RpcServiceT + Send + Sync,
{
    type MethodResponse = S::MethodResponse;
    type NotificationResponse = S::NotificationResponse;
    type BatchResponse = S::BatchResponse;

    fn call<'a>(
        &self,
        request: jsonrpsee::types::Request<'a>,
    ) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        self.inner.call(request)
    }

    fn batch<'a>(&self, batch: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        self.inner.batch(batch)
    }

    fn notification<'a>(
        &self,
        n: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        self.inner.notification(n)
    }
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
async fn check_origin(
    State(origins): State<Origins>,
    mut req: Request,
    next: Next,
) -> Response<Body> {
    let acts = req.method() == axum::http::Method::POST || is_upgrade(req.headers());
    if acts && !origins.allow(req.headers()) {
        tracing::warn!(origin = ?req.headers().get(header::ORIGIN), "admin request from another origin refused");
        return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
    }
    // Allowed just now: the web UI's own origin, for the methods.
    if acts
        && let Some(origin) = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .filter(|o| o.starts_with("http://") || o.starts_with("https://"))
    {
        let origin = PageOrigin(origin.to_owned());
        req.extensions_mut().insert(origin);
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
    cookie(headers, COOKIE)
}

/// The cookie called `name`, if the request has it.
fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, value)| value.to_owned())
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
        error_code::ALREADY_SET_UP | error_code::USER_EXISTS | error_code::SETTING_LOCKED => {
            StatusCode::CONFLICT
        }
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
async fn signed_in(api: &AdminApi, headers: &HeaderMap, done: LoggedIn) -> Response<Body> {
    api.mark_web_session(&done.token).await;
    let cookies = api.cookies();
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        && !cookies.insecure()
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
        .insert(header::SET_COOKIE, cookies.set(&done.token));
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
        Ok(done) => signed_in(&api, &headers, done).await,
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
        Ok(done) => signed_in(&api, &headers, done).await,
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
        Ok(done) => signed_in(&api, &headers, done).await,
        Err(e) => failure(e),
    }
}

/// The wizard's first page (P3-10): the setup code from the log, and
/// whether to send insecure cookies from now on.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetupCheck {
    code: String,
    insecure_cookies: bool,
}

async fn setup_check(
    State(api): State<AdminApi>,
    Extension(peer): Extension<Peer>,
    body: axum::body::Bytes,
) -> Response<Body> {
    let check: SetupCheck = match parse(&body) {
        Ok(c) => c,
        Err(e) => return failure(e),
    };
    match api
        .setup_check(&from(peer), &check.code, check.insecure_cookies)
        .await
    {
        Ok(()) => axum::Json(json!({ "ok": true })).into_response(),
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
        Ok(done) => signed_in(&api, &headers, done).await,
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

/// Which provider, and what for (`signIn`, `link`, `confirm`); for a
/// sign-in, the page to go back to (path and query).
#[derive(serde::Deserialize)]
struct SsoBegin {
    provider: String,
    purpose: String,
    #[serde(default)]
    return_to: Option<String>,
}

/// The most a request body to `/auth/oidc/begin` may be.
const SMALL_BODY: usize = 4096;

/// Begin a sign-in provider flow (P3-04): where to send the browser, as
/// `{"url": …}`, and the flow's cookie. Linking and confirming are in the
/// caller's session, so the session cookie is read as the socket reads it.
async fn sso_begin(State(api): State<AdminApi>, req: Request) -> Response<Body> {
    let mut ext = as_caller(&req);
    if let Some(peer) = req.extensions().get::<Peer>() {
        ext.insert(*peer);
    }
    let body = match axum::body::to_bytes(req.into_body(), SMALL_BODY).await {
        Ok(body) => body,
        Err(e) => {
            return failure(ErrorObjectOwned::owned::<()>(
                error_code::INVALID_INPUT,
                e.to_string(),
                None,
            ));
        }
    };
    let SsoBegin {
        provider,
        purpose,
        return_to,
    } = match parse(&body) {
        Ok(begin) => begin,
        Err(e) => return failure(e),
    };
    match api
        .sso_begin(&ext, &provider, &purpose, return_to.as_deref())
        .await
    {
        Ok(begun) => {
            let mut response = axum::Json(json!({ "url": begun.url })).into_response();
            response
                .headers_mut()
                .insert(header::SET_COOKIE, api.cookies().oidc(&begun.browser));
            response
        }
        Err(e) => failure(e),
    }
}

/// Where a provider sends the browser back (P3-04): `state` and `code`,
/// or `error`. Always a redirect: signed in to the page the sign-in began
/// on (as the begin named it, never this request) or the start page, or
/// back to the page that began it, saying what happened (`?sso=`). The flow's
/// cookie is spent either way.
async fn sso_callback(State(api): State<AdminApi>, req: Request) -> Response<Body> {
    let mut state = None;
    let mut code = None;
    for (key, value) in
        url::form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes())
    {
        match key.as_ref() {
            "state" => state = Some(value.into_owned()),
            "code" => code = Some(value.into_owned()),
            _ => {}
        }
    }
    let browser = cookie(req.headers(), OIDC_COOKIE);
    let outcome = match state {
        Some(state) => {
            api.sso_callback(&state, code.as_deref(), browser.as_deref())
                .await
        }
        None => SsoOutcome::Back("/login?sso=expired".into()),
    };
    let (location, session) = match outcome {
        SsoOutcome::SignedIn { done, to } => (to, Some(done.token)),
        SsoOutcome::Back(to) => (to, None),
    };
    let mut response = (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            HeaderValue::from_str(&location).expect("a path we made"),
        )],
    )
        .into_response();
    let headers = response.headers_mut();
    headers.append(header::SET_COOKIE, api.cookies().oidc_clear());
    if let Some(token) = session {
        api.mark_web_session(&token).await;
        headers.append(header::SET_COOKIE, api.cookies().set(&token));
    }
    // The page's address has the code in it; it is not sent on.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
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
        .insert(header::SET_COOKIE, api.cookies().clear());
    response
}

/// Who the cookie signs in, read now (role included): the web UI asks
/// before opening the socket, whose refusal a browser cannot read. No one
/// is 401, unless the caller asks with `?or=null` (the web UI does): then
/// `null`, as being signed out is an answer the page expects, and a 401
/// is logged in the browser's console as a failed request.
async fn me(State(api): State<AdminApi>, req: Request) -> Response<Body> {
    let or_null = asks_for_null(req.uri().query());
    match MeApiServer::who_am_i(&api, &as_caller(&req)).await {
        Ok(user) => axum::Json(user).into_response(),
        Err(e) if or_null && e.code() == error_code::UNAUTHENTICATED => {
            axum::Json(serde_json::Value::Null).into_response()
        }
        Err(e) => failure(e),
    }
}

/// Whether a `/auth/me` query asks for `null` instead of 401: `or=null`.
fn asks_for_null(query: Option<&str>) -> bool {
    query.is_some_and(|q| {
        url::form_urlencoded::parse(q.as_bytes()).any(|(key, value)| key == "or" && value == "null")
    })
}

/// Which artwork a catalog page shows (`id`), and at what size (`size`,
/// a thumb if left out) (P3-16).
fn artwork_query(query: Option<&str>) -> Option<(String, spoolframe_protocol::ArtworkSize)> {
    let mut id = None;
    let mut size = spoolframe_protocol::ArtworkSize::Thumb;
    for (key, value) in url::form_urlencoded::parse(query?.as_bytes()) {
        match &*key {
            "id" => id = Some(value.into_owned()),
            "size" => {
                size = serde_json::from_value(serde_json::Value::String(value.into_owned()))
                    .ok()
                    .filter(|s| *s != spoolframe_protocol::ArtworkSize::Unknown)?;
            }
            _ => {}
        }
    }
    Some((id?, size))
}

/// Artwork for the catalog pages (P3-16), to admins only: an `<img>`
/// cannot speak JSON-RPC, and carries the session cookie as any request
/// to this origin does. Artwork ids change with the image, so a browser
/// may keep it.
async fn catalog_artwork(State(api): State<AdminApi>, req: Request) -> Response<Body> {
    let Some((id, size)) = artwork_query(req.uri().query()) else {
        return (StatusCode::BAD_REQUEST, "id, and a size clients use").into_response();
    };
    match api.catalog_image(&as_caller(&req), &id, size).await {
        Ok(image) if !is_raster(&image.content_type) => {
            // An SVG runs script when opened by itself (PX-11, PX-09
            // finding 13); nothing but the three is served to a browser.
            tracing::warn!(
                id,
                content_type = image.content_type,
                "artwork that is not JPEG, PNG or WebP is not served"
            );
            (
                StatusCode::BAD_GATEWAY,
                "the artwork is not a JPEG, PNG or WebP image",
            )
                .into_response()
        }
        Ok(image) => {
            let mut response = (
                [
                    (header::CONTENT_TYPE, image.content_type),
                    (
                        header::CACHE_CONTROL,
                        "private, max-age=86400, immutable".to_owned(),
                    ),
                ],
                image.bytes,
            )
                .into_response();
            if let Some(etag) = image
                .etag
                .and_then(|e| HeaderValue::from_str(&format!("\"{e}\"")).ok())
            {
                response.headers_mut().insert(header::ETAG, etag);
            }
            response
        }
        Err(e) => failure(e),
    }
}

/// Whether `content_type` is one of the raster types artwork is served
/// as: JPEG, PNG or WebP, whatever its case or parameters.
fn is_raster(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(essence.as_str(), "image/jpeg" | "image/png" | "image/webp")
}

/// `GET /metrics` (P3-14): Prometheus text, for a scraper at one of
/// `admin.metrics_from`'s addresses. Not found while `admin.metrics` is
/// off, so the listener does not even say it could; refused from anywhere
/// else. The caller is who a trusted proxy says it carries: a proxy on
/// this machine does not make the Internet loopback.
async fn metrics_page(State(api): State<AdminApi>, req: Request) -> Response<Body> {
    let Some(metrics) = api.metrics().filter(|m| m.access().enabled()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let caller = req.extensions().get::<Peer>().and_then(|peer| peer.0);
    if !caller.is_some_and(|ip| metrics.access().allows(ip)) {
        tracing::debug!(
            ?caller,
            "metrics refused to an address not in admin.metrics_from"
        );
        if let Some(ip) = caller {
            metrics
                .access()
                .refused(ip, crate::metrics::history::unix_ms());
        }
        return (StatusCode::FORBIDDEN, "not from this address").into_response();
    }
    (
        [(header::CONTENT_TYPE, crate::metrics::CONTENT_TYPE)],
        crate::metrics::render(&metrics.families()),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn who_is_signed_in_answers_null_for_no_one_only_when_asked() {
        assert_eq!(
            [None, Some("or=null"), Some("x=1&or=null"), Some("or=401")].map(asks_for_null),
            [false, true, true, false]
        );
    }

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
        let cookies = CookiePolicy::default();
        for cookie in [cookies.set("abc"), cookies.clear()] {
            let cookie = cookie.to_str().unwrap();
            for part in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/"] {
                assert!(cookie.contains(part), "{cookie}");
            }
        }
    }

    /// A provider flow's cookie (P3-04) goes as the session's does: over
    /// plain HTTP too when insecure cookies are on, or the flow could not
    /// finish there. Lax either way, for the provider's redirect back.
    #[test]
    fn the_flow_cookie_follows_the_cookie_policy_and_is_lax() {
        let secure = CookiePolicy::default();
        let insecure = CookiePolicy::new(true);
        for cookie in [secure.oidc("ab"), secure.oidc_clear()] {
            let cookie = cookie.to_str().unwrap();
            for part in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/auth/oidc"] {
                assert!(cookie.contains(part), "{cookie}");
            }
        }
        for cookie in [insecure.oidc("ab"), insecure.oidc_clear()] {
            let cookie = cookie.to_str().unwrap();
            assert!(!cookie.contains("Secure"), "{cookie}");
            assert!(cookie.contains("HttpOnly"), "{cookie}");
            assert!(cookie.contains("SameSite=Lax"), "{cookie}");
        }
    }

    /// Insecure cookies drop only `Secure`, which plain HTTP needs gone:
    /// page scripts still cannot read them (PX-11, PX-09 finding 21).
    #[test]
    fn an_insecure_cookie_is_not_secure_but_still_http_only_and_strict() {
        let cookies = CookiePolicy::new(true);
        for cookie in [cookies.set("abc"), cookies.clear()] {
            let cookie = cookie.to_str().unwrap();
            assert!(!cookie.contains("Secure"), "{cookie}");
            assert!(cookie.contains("HttpOnly"), "{cookie}");
            assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        }
        // Live: the next sign-in reads it again.
        cookies.set_insecure(false);
        assert!(cookies.set("abc").to_str().unwrap().contains("Secure"));
    }
}
