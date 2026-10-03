//! The `/auth` endpoints: plain HTTP on the page's own origin, so the
//! browser sends and keeps the session cookie by itself.

use gloo_net::http::{Request, Response};
use serde::Deserialize;
use serde_json::json;
use spoolframe_admin_api::{Role, UserInfo};

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

pub async fn setup_open() -> Result<bool, ApiError> {
    #[derive(Deserialize)]
    struct Open {
        open: bool,
    }
    let response = Request::get("/auth/setup")
        .send()
        .await
        .map_err(unreachable)?;
    if !response.ok() {
        return Err(error_of(response).await);
    }
    response
        .json::<Open>()
        .await
        .map(|o| o.open)
        .map_err(|e| ApiError::transport(e.to_string()))
}
