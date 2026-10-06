//! The `/auth` endpoints: plain HTTP on the page's own origin, so the
//! browser sends and keeps the session cookie by itself.

use gloo_net::http::{Request, Response};
use serde::Deserialize;
use serde_json::json;
use spoolframe_admin_api::{PasskeyChallenge, Role, UserInfo};

use crate::logic::ApiError;

/// Who signed in, as `/auth/login` and `/auth/setup` answer.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SignedIn {
    pub username: String,
    pub role: Role,
}

async fn error_of(response: Response) -> ApiError {
    response
        .json::<ApiError>()
        .await
        .unwrap_or_else(|_| ApiError::transport(format!("HTTP {}", response.status())))
}

fn unreachable(e: gloo_net::Error) -> ApiError {
    ApiError::transport(e.to_string())
}

async fn post(path: &str, body: serde_json::Value) -> Result<Response, ApiError> {
    Request::post(path)
        .json(&body)
        .map_err(unreachable)?
        .send()
        .await
        .map_err(unreachable)
}

async fn signed_in(response: Response) -> Result<SignedIn, ApiError> {
    if !response.ok() {
        return Err(error_of(response).await);
    }
    response
        .json()
        .await
        .map_err(|e| ApiError::transport(e.to_string()))
}

pub async fn login(username: &str, password: &str) -> Result<SignedIn, ApiError> {
    let response = post(
        "/auth/login",
        json!({ "username": username, "password": password }),
    )
    .await?;
    signed_in(response).await
}

/// The server as a signed-out page knows it (`/auth/site`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Site {
    pub name: String,
    /// `admin.public_url`, where a browser is sent when its own address
    /// cannot keep the session.
    pub public_url: Option<String>,
    /// `admin.insecure_cookies` (P3-10): the session cookie works over
    /// plain HTTP, and every admin page says so.
    pub insecure_cookies: bool,
    /// The sign-in providers' names (P3-04), in the config's order: one
    /// "Continue with" button each. Empty: no SSO anywhere.
    pub sso: Vec<String>,
}

pub async fn site() -> Result<Site, ApiError> {
    let response = Request::get("/auth/site")
        .send()
        .await
        .map_err(unreachable)?;
    if !response.ok() {
        return Err(error_of(response).await);
    }
    response
        .json()
        .await
        .map_err(|e| ApiError::transport(e.to_string()))
}

/// Begin a flow with sign-in provider `provider` (P3-04) for `purpose`
/// (`signIn`, `link`, `confirm`): the provider's address to send the
/// browser to. The flow's cookie comes with the answer. A sign-in may
/// name the page to come back to (`return_to`, path and query); without
/// one, it comes back to the start page.
pub async fn sso_begin(
    provider: &str,
    purpose: &str,
    return_to: Option<&str>,
) -> Result<String, ApiError> {
    let body = json!({ "provider": provider, "purpose": purpose, "return_to": return_to });
    let response = post("/auth/oidc/begin", body).await?;
    if !response.ok() {
        return Err(error_of(response).await);
    }
    #[derive(Deserialize)]
    struct Begun {
        url: String,
    }
    response
        .json::<Begun>()
        .await
        .map(|b| b.url)
        .map_err(|e| ApiError::transport(e.to_string()))
}

/// Send the browser to `url`: to a sign-in provider, which sends it back.
pub fn go(url: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.location().set_href(url);
    }
}

thread_local! {
    /// What the server's SSO callback said, read from the address the app
    /// opened at, with that address's path.
    static SSO_OUTCOME: std::cell::RefCell<Option<(String, String, Option<String>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Read what the server's SSO callback said (`?sso=…&provider=…`) from the
/// address the app opens at, before the router sees it, and take it out
/// of the address, so a reload does not say it again. The page's own
/// query stays.
pub fn capture_sso_outcome() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let location = window.location();
    let (Ok(search), Ok(path)) = (location.search(), location.pathname()) else {
        return;
    };
    let Some((what, provider)) = crate::sso::outcome(&search) else {
        return;
    };
    let address = format!("{path}{}", crate::sso::without_outcome(&search));
    SSO_OUTCOME.with(|o| *o.borrow_mut() = Some((path, what, provider)));
    if let Ok(history) = window.history() {
        let _ = history.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&address));
    }
}

/// What the SSO callback said, once, if the app opened at `path`.
pub fn sso_outcome(path: &str) -> Option<(String, Option<String>)> {
    SSO_OUTCOME.with(|o| {
        let mut o = o.borrow_mut();
        match o.as_ref() {
            Some((at, _, _)) if at == path => o.take().map(|(_, what, provider)| (what, provider)),
            _ => None,
        }
    })
}

/// Begin a passkey sign-in: the request's id, and the options for the
/// browser's prompt.
pub async fn passkey_begin() -> Result<PasskeyChallenge, ApiError> {
    let response = post("/auth/passkey/begin", json!({})).await?;
    if !response.ok() {
        return Err(error_of(response).await);
    }
    response
        .json()
        .await
        .map_err(|e| ApiError::transport(e.to_string()))
}

/// Finish it with what the passkey signed.
pub async fn passkey_finish(
    ceremony: &str,
    credential: &serde_json::Value,
) -> Result<SignedIn, ApiError> {
    let response = post(
        "/auth/passkey/finish",
        json!({ "ceremony": ceremony, "credential": credential }),
    )
    .await?;
    signed_in(response).await
}

/// Begin creating the first admin with a passkey: the request's id, and
/// the options for the browser's prompt.
pub async fn setup_passkey_begin(code: &str, username: &str) -> Result<PasskeyChallenge, ApiError> {
    let response = post(
        "/auth/setup/passkey/begin",
        json!({ "code": code, "username": username }),
    )
    .await?;
    if !response.ok() {
        return Err(error_of(response).await);
    }
    response
        .json()
        .await
        .map_err(|e| ApiError::transport(e.to_string()))
}

/// Finish it with what the browser made: the admin exists, signed in.
pub async fn setup_passkey_finish(
    code: &str,
    ceremony: &str,
    name: &str,
    credential: &serde_json::Value,
) -> Result<SignedIn, ApiError> {
    let response = post(
        "/auth/setup/passkey/finish",
        json!({ "code": code, "ceremony": ceremony, "name": name, "credential": credential }),
    )
    .await?;
    signed_in(response).await
}

/// The wizard's first page (P3-10): whether `code` is the setup code, and
/// with `insecure_cookies`, the session cookie sent so that plain HTTP
/// keeps it from now on.
pub async fn setup_check(code: &str, insecure_cookies: bool) -> Result<(), ApiError> {
    let response = post(
        "/auth/setup/check",
        json!({ "code": code, "insecureCookies": insecure_cookies }),
    )
    .await?;
    if response.ok() {
        Ok(())
    } else {
        Err(error_of(response).await)
    }
}

pub async fn setup(code: &str, username: &str, password: &str) -> Result<SignedIn, ApiError> {
    let response = post(
        "/auth/setup",
        json!({ "code": code, "username": username, "password": password }),
    )
    .await?;
    signed_in(response).await
}

pub async fn logout() -> Result<(), ApiError> {
    let response = post("/auth/logout", json!({})).await?;
    if response.ok() {
        Ok(())
    } else {
        Err(error_of(response).await)
    }
}

/// Who the cookie signs in; `Ok(None)` when no one. Asked before the socket
/// is opened: a refused WebSocket tells a browser nothing it can read.
pub async fn me() -> Result<Option<UserInfo>, ApiError> {
    // `or=null`: no one signed in is `null`, not a 401 the browser would
    // log as a failed request on every signed-out page.
    let response = Request::get("/auth/me?or=null")
        .send()
        .await
        .map_err(unreachable)?;
    match response.status() {
        200 => response
            .json()
            .await
            .map_err(|e| ApiError::transport(e.to_string())),
        // A server from before `or=null`.
        401 => Ok(None),
        _ => Err(error_of(response).await),
    }
}

/// Whether the first admin can still be set up (`/auth/setup`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupState {
    pub open: bool,
    /// Why the server cannot make the admin a passkey, if it cannot.
    #[serde(deserialize_with = "spoolframe_types::required::option")]
    pub passkeys_unavailable: Option<String>,
}

pub async fn setup_state() -> Result<SetupState, ApiError> {
    let response = Request::get("/auth/setup")
        .send()
        .await
        .map_err(unreachable)?;
    if !response.ok() {
        return Err(error_of(response).await);
    }
    response
        .json()
        .await
        .map_err(|e| ApiError::transport(e.to_string()))
}

pub async fn setup_open() -> Result<bool, ApiError> {
    setup_state().await.map(|state| state.open)
}
