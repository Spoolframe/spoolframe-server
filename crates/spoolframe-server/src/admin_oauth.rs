//! The OAuth endpoints MCP clients sign in through (PX-42), on the admin
//! listener, when `mcp.enabled` is on; off, each is not found.
//!
//! Two kinds, routed apart:
//! - The browser's: `/authorize`, which the client sends its user's
//!   browser to, and `/oauth/pending/{id}` and `/oauth/decide`, which the
//!   web UI's consent page (`/connect`) calls with the session cookie.
//!   `/authorize` needs no cookie: the browser arrives from the client's
//!   site, and a `SameSite=Strict` cookie is not sent on that navigation.
//!   It checks the request, keeps it, and sends the browser on to the
//!   consent page, whose own fetches carry the cookie.
//! - The client's: the metadata documents, `/register`, `/token` and
//!   `/mcp` itself. Called from the client's servers or a browser on
//!   its site, so not held to the web UI's origin, with CORS open to any
//!   origin and never a cookie read: a token is the only credential.

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, Response, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{get, post, post_service};
use axum::{Extension, Router};
use jsonrpsee::types::ErrorObjectOwned;
use serde_json::json;
use spoolframe_admin_api::error_code;

use super::{
    AUTH_BODY_LIMIT, TrustedProxies, as_caller, failure, own_host, parse, refused, request_timeout,
    security_headers,
};
use crate::admin::{AdminApi, Peer};
use crate::oauth::{AuthorizeError, AuthorizeRequest, RegisterError, Scope, TokenError};

/// Where the consent page is: the web UI's, outside its shell.
const CONSENT_PAGE: &str = "/connect";
/// Where the protected resource's metadata is (RFC 9728), for `/mcp`.
pub(crate) const RESOURCE_METADATA: &str = "/.well-known/oauth-protected-resource/mcp";

/// The browser's endpoints, beside the web UI's own: held to its origin
/// and its cookie, as every other route there is.
pub(super) fn browser_routes() -> Router<AdminApi> {
    Router::new()
        .route("/authorize", get(authorize))
        .route("/oauth/pending/{id}", get(pending))
        .route(
            "/oauth/decide",
            post(decide).layer(axum::extract::DefaultBodyLimit::max(AUTH_BODY_LIMIT)),
        )
}

/// The client's endpoints, with layers of their own: the caller's
/// address, open CORS, the timeout and the security headers; not the web
/// UI's origin check, nor its cookie.
pub(super) fn client_routes(api: AdminApi, proxies: TrustedProxies) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(RESOURCE_METADATA, get(protected_resource))
        .route("/register", post(register))
        .route("/token", post(token))
        .route(
            "/mcp",
            post_service(mcp_service(api.clone()))
                .route_layer(middleware::from_fn_with_state(api.clone(), bearer)),
        )
        .layer(axum::extract::DefaultBodyLimit::max(AUTH_BODY_LIMIT))
        .with_state(api.clone())
        .layer(middleware::from_fn(cors))
        .layer(middleware::from_fn_with_state(api, when_on))
        .layer(middleware::from_fn_with_state(proxies, super::peer))
        .layer(middleware::from_fn(request_timeout))
        .layer(middleware::from_fn(security_headers))
}

/// The most an MCP call's body may be: a tool's arguments are a few
/// hundred bytes.
const MCP_BODY_LIMIT: usize = 64 * 1024;

/// `/mcp`: MCP's Streamable HTTP transport, stateless, every answer JSON.
/// Nothing streams, so a call is held to the listener's request timeout
/// as any other, and `GET` (a stream from the server) is not offered.
/// Its own `Host` and `Origin` checks are off: the access token is what
/// lets a call in, which no other site's page has, and behind a proxy
/// `Host` is the proxy's to set.
fn mcp_service(
    api: AdminApi,
) -> rmcp::transport::streamable_http_server::StreamableHttpService<
    crate::mcp::McpServer,
    rmcp::transport::streamable_http_server::session::never::NeverSessionManager,
> {
    use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
    rmcp::transport::streamable_http_server::StreamableHttpService::new(
        move || Ok(crate::mcp::McpServer::new(api.clone())),
        std::sync::Arc::default(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_sse_keep_alive(None)
            .with_sse_retry(None)
            .disable_allowed_hosts()
            .with_max_request_body_bytes(MCP_BODY_LIMIT),
    )
}

/// Only a live access token reaches `/mcp`: its user and scopes, read
/// now, go with the request. Without one, 401 and where to find how to
/// sign in (RFC 9728 §5.1), which is how a client connecting by the URL
/// alone starts. Never a cookie: a page of the web UI's is no MCP client.
async fn bearer(State(api): State<AdminApi>, mut req: Request, next: Next) -> Response<Body> {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.trim().to_owned());
    let access = match &token {
        Some(token) => api.oauth().access(token).await,
        None => Ok(None),
    };
    match access {
        Ok(Some(access)) => {
            req.extensions_mut().insert(crate::mcp::McpCaller {
                user: access.user,
                admin: access.role.is_admin(),
                client_name: access.client_name,
                grant_id: access.grant_id,
                scopes: crate::oauth::parse_scopes(&access.scopes),
            });
            next.run(req).await
        }
        Ok(None) => {
            let mut challenge = String::from("Bearer");
            if let Some(issuer) = issuer(&api, req.headers()) {
                challenge.push_str(&format!(
                    " resource_metadata=\"{issuer}{RESOURCE_METADATA}\""
                ));
            }
            if token.is_some() {
                challenge.push_str(", error=\"invalid_token\"");
            }
            let mut response = (StatusCode::UNAUTHORIZED, "sign in first").into_response();
            if let Ok(value) = HeaderValue::from_str(&challenge) {
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, value);
            }
            response
        }
        Err(e) => {
            tracing::error!(error = %e, "cannot check an MCP access token");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// Not found while `mcp.enabled` is off: the server does not even say it
/// could.
async fn when_on(State(api): State<AdminApi>, req: Request, next: Next) -> Response<Body> {
    if !api.mcp_on() {
        return StatusCode::NOT_FOUND.into_response();
    }
    next.run(req).await
}

/// CORS for any origin, without credentials: a client in a browser on its
/// own site may call, and its browser sends no cookie, as none is read.
/// A preflight is answered here.
async fn cors(req: Request, next: Next) -> Response<Body> {
    let mut response = if req.method() == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, DELETE, OPTIONS"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static(
                "authorization, content-type, accept, mcp-protocol-version, mcp-session-id, \
                 last-event-id",
            ),
        );
        headers.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("86400"),
        );
        response
    } else {
        next.run(req).await
    };
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("www-authenticate, mcp-session-id, mcp-protocol-version"),
    );
    response
}

/// The authorization server's identifier, and the base of every URL the
/// metadata names: `admin.public_url`, or else the address the request
/// was sent to, if that can only be this listener (an address, or
/// `localhost`). A name could be anyone's DNS, and the metadata would then
/// send the client's user to whoever it points at.
pub(crate) fn issuer(api: &AdminApi, headers: &HeaderMap) -> Option<String> {
    if let Some(url) = api.public_url() {
        return Some(url.trim_end_matches('/').to_owned());
    }
    headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| own_host(h))
        .map(|h| format!("http://{h}"))
}

/// What a request gets when no issuer can be named.
fn no_issuer() -> Response<Body> {
    (
        StatusCode::BAD_REQUEST,
        "reach the server by its address, or set admin.public_url",
    )
        .into_response()
}

/// The scopes the metadata lists: every one the server has.
fn scopes_supported() -> Vec<&'static str> {
    Scope::ALL.iter().map(|s| s.as_str()).collect()
}

/// RFC 8414.
async fn authorization_server(State(api): State<AdminApi>, headers: HeaderMap) -> Response<Body> {
    let Some(issuer) = issuer(&api, &headers) else {
        return no_issuer();
    };
    axum::Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "registration_endpoint": format!("{issuer}/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": scopes_supported(),
    }))
    .into_response()
}

/// RFC 9728: `/mcp` is the resource, and this server its authorization
/// server.
async fn protected_resource(State(api): State<AdminApi>, headers: HeaderMap) -> Response<Body> {
    let Some(issuer) = issuer(&api, &headers) else {
        return no_issuer();
    };
    axum::Json(json!({
        "resource": format!("{issuer}/mcp"),
        "authorization_servers": [issuer],
        "scopes_supported": scopes_supported(),
        "bearer_methods_supported": ["header"],
        "resource_name": "Spoolframe",
    }))
    .into_response()
}

/// A registration request (RFC 7591 §2): what is read of it.
#[derive(serde::Deserialize)]
struct Registration {
    client_name: Option<String>,
    redirect_uris: Vec<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
}

/// An OAuth error answer (RFC 6749 §5.2, RFC 7591 §3.2.2).
fn oauth_error(status: StatusCode, code: &str, description: &str) -> Response<Body> {
    (
        status,
        axum::Json(json!({ "error": code, "error_description": description })),
    )
        .into_response()
}

/// `POST /register`: a public client (no secret: `token_endpoint_auth_method`
/// is always `none`, whatever was asked), with the authorization code and
/// refresh tokens.
async fn register(
    State(api): State<AdminApi>,
    Extension(Peer(address)): Extension<Peer>,
    body: axum::body::Bytes,
) -> Response<Body> {
    let request: Registration = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_client_metadata",
                &e.to_string(),
            );
        }
    };
    let grants_ok = request.grant_types.as_ref().is_none_or(|g| {
        g.iter()
            .all(|g| g == "authorization_code" || g == "refresh_token")
    });
    let responses_ok = request
        .response_types
        .as_ref()
        .is_none_or(|r| r.iter().all(|r| r == "code"));
    if !grants_ok || !responses_ok {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "only the authorization code (with refresh tokens) is offered",
        );
    }
    if !api.oauth().may_register(address) {
        tracing::warn!(
            ?address,
            "OAuth registrations refused: too many from one address"
        );
        return oauth_error(
            StatusCode::TOO_MANY_REQUESTS,
            "invalid_client_metadata",
            "too many registrations from this address; try again later",
        );
    }
    match api
        .oauth()
        .register(request.client_name.as_deref(), &request.redirect_uris)
        .await
    {
        Ok(client) => (
            StatusCode::CREATED,
            axum::Json(json!({
                "client_id": client.id,
                "client_id_issued_at": client.created_at,
                "client_name": client.name,
                "redirect_uris": client.redirect_uris,
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
            })),
        )
            .into_response(),
        Err(RegisterError::Invalid { code, message }) => {
            oauth_error(StatusCode::BAD_REQUEST, code, &message)
        }
        Err(RegisterError::Store(e)) => {
            tracing::error!(error = %e, "cannot register an OAuth client");
            oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "internal error",
            )
        }
    }
}

/// A form's or a query's fields, or `None` when one is given twice:
/// OAuth's parameters must not repeat (RFC 6749 §3.1).
fn fields(text: &str) -> Option<HashMap<String, String>> {
    let mut fields = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(text.as_bytes()) {
        if fields
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return None;
        }
    }
    Some(fields)
}

/// The server's own page saying why a request cannot be used: 400, and
/// no `Location`, so nothing reaches the address the request named.
fn broken_page(why: &str) -> Response<Body> {
    let why = html_escape(why);
    let page = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Cannot connect the app</title>\
         <style>body{{font-family:system-ui,sans-serif;background:#1e1e2e;color:#cdd6f4;\
         display:grid;place-items:center;min-height:100vh;margin:0}}\
         main{{max-width:30rem;padding:2rem}}h1{{font-size:1.4rem}}p{{color:#bac2de;line-height:1.5}}</style>\
         </head><body><main><h1>This request to connect an app cannot be used</h1>\
         <p>{why}</p>\
         <p>Nothing was sent back to the app. Start connecting again from the app. If this \
         happens again, the app may not work with this server.</p></main></body></html>"
    );
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        page,
    )
        .into_response()
}

fn html_escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' => "&amp;".to_owned(),
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '"' => "&quot;".to_owned(),
            '\'' => "&#39;".to_owned(),
            c => c.to_string(),
        })
        .collect()
}

/// Whether `resource` (RFC 8707), when the client names one, is on this
/// server: the issuer's origin.
fn our_resource(resource: Option<&str>, issuer: &str) -> bool {
    let Some(resource) = resource else {
        return true;
    };
    match (url::Url::parse(resource), url::Url::parse(issuer)) {
        (Ok(resource), Ok(issuer)) => resource.origin() == issuer.origin(),
        _ => false,
    }
}

/// `GET /authorize`: check the request, keep it, and send the browser to
/// the consent page; or say on the server's own page why not.
async fn authorize(State(api): State<AdminApi>, req: Request) -> Response<Body> {
    if !api.mcp_on() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(issuer) = issuer(&api, req.headers()) else {
        return no_issuer();
    };
    let Some(mut query) = fields(req.uri().query().unwrap_or_default()) else {
        return broken_page("The app sent a parameter twice.");
    };
    let request = AuthorizeRequest {
        response_type: query.remove("response_type"),
        client_id: query.remove("client_id"),
        redirect_uri: query.remove("redirect_uri"),
        scope: query.remove("scope"),
        state: query.remove("state"),
        code_challenge: query.remove("code_challenge"),
        code_challenge_method: query.remove("code_challenge_method"),
        resource: query.remove("resource"),
    };
    if !our_resource(request.resource.as_deref(), &issuer) {
        return broken_page("The app asked for access to another server.");
    }
    match api.oauth().authorize(&request).await {
        Ok(id) => (
            StatusCode::SEE_OTHER,
            [(
                header::LOCATION,
                HeaderValue::from_str(&format!("{CONSENT_PAGE}?request={id}"))
                    .expect("the id is hex"),
            )],
        )
            .into_response(),
        Err(AuthorizeError::Broken(why)) => {
            tracing::info!(why, client_id = ?request.client_id, "an OAuth authorization request was refused");
            broken_page(&why)
        }
        Err(AuthorizeError::Store(e)) => {
            tracing::error!(error = %e, "cannot read an OAuth client");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// `GET /oauth/pending/{id}`: what the consent page shows, for the
/// signed-in user.
async fn pending(
    State(api): State<AdminApi>,
    Path(id): Path<String>,
    req: Request,
) -> Response<Body> {
    if !api.mcp_on() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let ext = as_caller(&req);
    match api.oauth_pending(&ext, &id).await {
        Ok(view) => axum::Json(json!({
            "clientName": view.client_name,
            "returnHost": view.return_host,
            "scopes": view.scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => refused(&api, "/oauth/pending", &ext, e).await,
    }
}

/// The consent page's answer.
#[derive(serde::Deserialize)]
struct Decision {
    request: String,
    allow: bool,
    scopes: Vec<String>,
}

/// `POST /oauth/decide`: the user's answer, and where the page sends the
/// browser (`{"redirect": …}`): a fetch, as the page's content policy
/// lets a form post only to its own origin.
async fn decide(State(api): State<AdminApi>, req: Request) -> Response<Body> {
    if !api.mcp_on() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let ext = as_caller(&req);
    let body = match axum::body::to_bytes(req.into_body(), AUTH_BODY_LIMIT).await {
        Ok(body) => body,
        Err(e) => {
            return failure(ErrorObjectOwned::owned::<()>(
                error_code::INVALID_INPUT,
                e.to_string(),
                None,
            ));
        }
    };
    let decision: Decision = match parse(&body) {
        Ok(d) => d,
        Err(e) => return failure(e),
    };
    match api
        .oauth_decide(&ext, &decision.request, decision.allow, &decision.scopes)
        .await
    {
        Ok(to) => {
            let mut response = axum::Json(json!({ "redirect": to })).into_response();
            // The answer names a code.
            response.headers_mut().insert(
                header::REFERRER_POLICY,
                HeaderValue::from_static("no-referrer"),
            );
            response
        }
        Err(e) => refused(&api, "/oauth/decide", &ext, e).await,
    }
}

/// `POST /token`: a code or a refresh token for a new pair.
async fn token(State(api): State<AdminApi>, body: axum::body::Bytes) -> Response<Body> {
    let Some(mut form) = std::str::from_utf8(&body).ok().and_then(fields) else {
        return token_error(&TokenError::InvalidRequest(
            "a form, with each parameter once".into(),
        ));
    };
    let mut take = |name: &str| {
        form.remove(name)
            .ok_or_else(|| TokenError::InvalidRequest(format!("{name} is missing")))
    };
    let issued = async {
        let grant_type = take("grant_type")?;
        let client_id = take("client_id")?;
        match grant_type.as_str() {
            "authorization_code" => {
                let code = take("code")?;
                let redirect_uri = take("redirect_uri")?;
                let verifier = take("code_verifier")?;
                api.oauth()
                    .exchange(&client_id, &code, &redirect_uri, &verifier)
                    .await
            }
            "refresh_token" => {
                let refresh = take("refresh_token")?;
                api.oauth().refresh(&client_id, &refresh).await
            }
            _ => Err(TokenError::UnsupportedGrantType),
        }
    };
    match issued.await {
        Ok(tokens) => {
            let mut response = axum::Json(json!({
                "access_token": tokens.access_token,
                "token_type": "Bearer",
                "expires_in": tokens.expires_in,
                "refresh_token": tokens.refresh_token,
                "scope": tokens.scope,
            }))
            .into_response();
            response
                .headers_mut()
                .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
            response
        }
        Err(e) => token_error(&e),
    }
}

fn token_error(e: &TokenError) -> Response<Body> {
    let status = match e {
        TokenError::InvalidClient => StatusCode::UNAUTHORIZED,
        TokenError::Store(e) => {
            tracing::error!(error = %e, "the OAuth token endpoint");
            StatusCode::INTERNAL_SERVER_ERROR
        }
        _ => StatusCode::BAD_REQUEST,
    };
    let description = match e {
        TokenError::Store(_) => "internal error".to_owned(),
        e => e.to_string(),
    };
    oauth_error(status, e.code(), &description)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parameter_given_twice_is_refused() {
        assert_eq!(fields("a=1&b=2").unwrap()["b"], "2");
        assert_eq!(fields("a=1&a=2"), None);
    }

    #[test]
    fn a_resource_must_be_on_this_server() {
        let issuer = "https://media.example.org";
        assert!(our_resource(None, issuer));
        assert!(our_resource(Some("https://media.example.org/mcp"), issuer));
        assert!(!our_resource(Some("https://evil.example/mcp"), issuer));
        assert!(!our_resource(Some("http://media.example.org/mcp"), issuer));
    }

    #[test]
    fn the_broken_page_shows_text_not_markup() {
        assert_eq!(
            html_escape("<script>\"x\" & 'y'"),
            "&lt;script&gt;&quot;x&quot; &amp; &#39;y&#39;"
        );
    }
}
