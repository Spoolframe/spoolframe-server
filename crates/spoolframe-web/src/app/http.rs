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
    let response = Request::get("/auth/me").send().await.map_err(unreachable)?;
    match response.status() {
        200 => response
            .json()
            .await
            .map(Some)
            .map_err(|e| ApiError::transport(e.to_string())),
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
    #[serde(default)]
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
