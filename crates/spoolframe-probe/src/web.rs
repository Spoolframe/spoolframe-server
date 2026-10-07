//! The probe's side of the server's web listener: `/auth/login`, JSON-RPC
//! over `POST /jsonrpc` with the session cookie, and `/auth/logout`. As a
//! browser on the web UI's own page sends them: every POST carries its
//! `Origin`. Each request counts against the run's budget, and a 5xx is
//! a failed exchange, as an unanswered QUIC request is.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use crate::Web;
use crate::wire::{Probe, Problem};

/// The admin API's JSON-RPC error codes the checks expect
/// (`spoolframe_admin_api::error_code`, which this permissive crate cannot
/// link).
pub mod code {
    /// No session, or one that is revoked or expired.
    pub const UNAUTHENTICATED: i64 = -32001;
    /// The session's user is not an admin.
    pub const FORBIDDEN: i64 = -32002;
}

/// A JSON-RPC answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Result(Value),
    Error { code: i64, message: String },
}

impl Answer {
    /// Short enough for a line of evidence. A result is told by its size
    /// only: it can hold other users' names.
    pub fn describe(&self) -> String {
        match self {
            Answer::Result(v) => format!("a result ({} bytes)", v.to_string().len()),
            Answer::Error { code, message } => format!("error {code} {message:?}"),
        }
    }
}

/// A web session the probe signed in. The cookie never leaves this struct.
pub struct WebSession {
    client: reqwest::Client,
    url: String,
    origin: String,
    cookie: String,
    ids: AtomicU64,
}

fn client(probe: &Probe) -> Result<reqwest::Client, Problem> {
    reqwest::Client::builder()
        .timeout(probe.reply_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| Problem::Failed(format!("no HTTP client: {e}")))
}

/// What came back, or the run's failure for it: a 5xx or no answer
/// counts towards the stop rule, anything else is the server answering.
async fn sent(
    probe: &Probe,
    what: &str,
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, Problem> {
    probe.spend()?;
    match request.send().await {
        Ok(response) if response.status().is_server_error() => {
            Err(probe.failed(format!("{what}: HTTP {}", response.status())))
        }
        Ok(response) => {
            probe.answered();
            Ok(response)
        }
        Err(e) => Err(probe.failed(format!("{what}: no answer: {e}"))),
    }
}

/// Sign in to the web UI as `username`, once: a refusal is not tried
/// again (a wrong password counts towards the name's lockout).
pub async fn login(
    probe: &Probe,
    web: &Web,
    username: &str,
    password: &str,
) -> Result<WebSession, Problem> {
    let client = client(probe)?;
    let url = web.url.trim_end_matches('/').to_owned();
    let response = sent(
        probe,
        "POST /auth/login",
        client
            .post(format!("{url}/auth/login"))
            .header("origin", &web.origin)
            .json(&json!({ "username": username, "password": password })),
    )
    .await?;
    if !response.status().is_success() {
        return Err(Problem::Failed(format!(
            "POST /auth/login: HTTP {}",
            response.status()
        )));
    }
    let cookie = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("spoolframe_session="))
        .and_then(|v| v.split(';').next())
        .map(str::to_owned)
        .ok_or_else(|| Problem::Failed("POST /auth/login set no session cookie".into()))?;
    Ok(WebSession {
        client,
        url,
        origin: web.origin.clone(),
        cookie,
        ids: AtomicU64::new(1),
    })
}

impl WebSession {
    /// `method` with `params`, over `POST /jsonrpc`.
    pub async fn call(
        &self,
        probe: &Probe,
        method: &str,
        params: Value,
    ) -> Result<Answer, Problem> {
        let id = self.ids.fetch_add(1, Ordering::AcqRel);
        let what = format!("{method} over POST /jsonrpc");
        let response = sent(
            probe,
            &what,
            self.client
                .post(format!("{}/jsonrpc", self.url))
                .header("origin", &self.origin)
                .header("cookie", &self.cookie)
                .json(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })),
        )
        .await?;
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|e| Problem::Failed(format!("{what}: HTTP {status}, not JSON: {e}")))?;
        if let Some(error) = body.get("error") {
            return Ok(Answer::Error {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            });
        }
        match body.get("result") {
            Some(result) => Ok(Answer::Result(result.clone())),
            None => Err(Problem::Failed(format!(
                "{what}: HTTP {status}, neither a result nor an error"
            ))),
        }
    }

    /// End this session: `POST /auth/logout`. Its HTTP status.
    pub async fn logout(&self, probe: &Probe) -> Result<u16, Problem> {
        let response = sent(
            probe,
            "POST /auth/logout",
            self.client
                .post(format!("{}/auth/logout", self.url))
                .header("origin", &self.origin)
                .header("cookie", &self.cookie)
                .json(&json!({})),
        )
        .await?;
        Ok(response.status().as_u16())
    }
}
