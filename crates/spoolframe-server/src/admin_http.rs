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
use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use jsonrpsee::Extensions;
use jsonrpsee::server::{Methods, ServerHandle, StopHandle, stop_channel};
use jsonrpsee::types::ErrorObjectOwned;
use serde_json::json;
use spoolframe_admin_api::{AdminApiServer, Credentials, MeApiServer, SetupRequest, error_code};

use crate::admin::{AdminApi, Bearer};
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

/// Serve `methods`, the `/auth` endpoints and the web UI's pages on
/// `listen`.
pub(crate) async fn serve(
    listen: SocketAddr,
    api: AdminApi,
    methods: Methods,
    origins: Origins,
) -> std::io::Result<(SocketAddr, ServerHandle)> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let addr = listener.local_addr()?;
    let (stop, handle) = stop_channel();
    let app = router(api, methods, stop.clone(), origins);
    tokio::spawn(async move {
        let shutdown = stop.shutdown();
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await
        {
            tracing::error!(error = %e, "the admin listener failed");
        }
    });
    Ok((addr, handle))
}

fn router(api: AdminApi, methods: Methods, stop: StopHandle, origins: Origins) -> Router {
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
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/setup", get(setup_open).post(setup))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .with_state(api)
        // The socket: `/jsonrpc` for the web UI, and any other path's
        // upgrade too, which is where the CLI has always connected.
        .route_service("/jsonrpc", rpc.clone())
        .fallback_service(tower::service_fn(move |req: Request| {
            let mut rpc = rpc.clone();
            async move {
                if is_upgrade(req.headers()) {
                    return tower::Service::call(&mut rpc, req).await;
                }
                Ok::<_, Infallible>(crate::web::page(req.uri().path()))
            }
        }))
        .layer(middleware::from_fn(credentials))
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
/// is not rescued by a cookie.
async fn credentials(mut req: Request, next: Next) -> Response<Body> {
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
        error_code::UNAUTHENTICATED | error_code::INVALID_CREDENTIALS => StatusCode::UNAUTHORIZED,
        error_code::FORBIDDEN | error_code::INVALID_SETUP_CODE => StatusCode::FORBIDDEN,
        error_code::RATE_LIMITED => StatusCode::TOO_MANY_REQUESTS,
        error_code::ALREADY_SET_UP | error_code::USER_EXISTS => StatusCode::CONFLICT,
        error_code::INVALID_INPUT => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let body = json!({ "code": e.code(), "message": e.message(), "data": e.data() });
    (status, axum::Json(body)).into_response()
}

/// Signed in: the cookie, and who.
fn signed_in(token: &str, body: serde_json::Value) -> Response<Body> {
    let mut response = axum::Json(body).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, set_cookie(token));
    response
}

/// The JSON body as `T`, or why not (answered as a 400).
fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ErrorObjectOwned> {
    serde_json::from_slice(body)
        .map_err(|e| ErrorObjectOwned::owned::<()>(error_code::INVALID_INPUT, e.to_string(), None))
}

async fn login(State(api): State<AdminApi>, body: axum::body::Bytes) -> Response<Body> {
    let credentials: Credentials = match parse(&body) {
        Ok(c) => c,
        Err(e) => return failure(e),
    };
    match MeApiServer::login(&api, credentials).await {
        Ok(done) => signed_in(
            &done.token,
            json!({ "username": done.username, "role": done.role }),
        ),
        Err(e) => failure(e),
    }
}

async fn setup(State(api): State<AdminApi>, body: axum::body::Bytes) -> Response<Body> {
    let request: SetupRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return failure(e),
    };
    match AdminApiServer::setup(&api, request).await {
        Ok(done) => signed_in(
            &done.token,
            json!({ "username": done.username, "role": done.role }),
        ),
        Err(e) => failure(e),
    }
}

/// `{"open": true}` while the first admin can still be set up: a signed-out
/// browser shows setup rather than login.
async fn setup_open(State(api): State<AdminApi>) -> Response<Body> {
    match api.setup_open().await {
        Ok(open) => axum::Json(json!({ "open": open })).into_response(),
        Err(e) => failure(e),
    }
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
