//! Sign in with Plex (P3-10): plex.tv's PIN flow, which gets a Plex
//! account's token without anyone typing it. The server asks plex.tv for a
//! PIN, the browser opens Plex's sign-in page with it (or the person types
//! its short code at plex.tv/link), and the server polls the PIN until
//! plex.tv hands it the token.
//!
//! The only part of this crate that reaches the Internet, and only while
//! someone signs in: the catalog itself talks to the PMS on the LAN alone
//! (ARCHITECTURE §32). Both base URLs are given, so tests use a stand-in.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use url::Url;

/// plex.tv's API.
pub const PLEX_TV: &str = "https://plex.tv";
/// Where a person approves a PIN.
pub const PLEX_APP: &str = "https://app.plex.tv";

const PRODUCT: &str = "Spoolframe";
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("cannot reach plex.tv: {0}")]
    Unreachable(String),
    #[error("plex.tv answered {0}")]
    Refused(String),
}

/// A PIN to approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub id: u64,
    /// The short code a person can type at plex.tv/link.
    pub code: String,
    /// Seconds until plex.tv forgets it.
    pub expires_in: Option<u64>,
    /// Plex's sign-in page for it.
    pub auth_url: String,
}

/// Where a PIN is.
#[derive(Clone, PartialEq, Eq)]
pub enum PinState {
    Waiting,
    /// Approved: the account's token. Never shown or logged.
    Linked(String),
    /// plex.tv has forgotten it.
    Expired,
}

impl fmt::Debug for PinState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PinState::Waiting => f.write_str("Waiting"),
            PinState::Linked(_) => f.write_str("Linked(<redacted>)"),
            PinState::Expired => f.write_str("Expired"),
        }
    }
}

/// plex.tv, as one app installation (`client_id`) sees it.
#[derive(Debug, Clone)]
pub struct PlexTv {
    api: Url,
    app: Url,
    client_id: String,
    http: reqwest::Client,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PinReply {
    id: u64,
    code: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    auth_token: Option<String>,
}

#[derive(Deserialize)]
struct UserReply {
    username: Option<String>,
    email: Option<String>,
}

impl PlexTv {
    /// plex.tv at `api`, its sign-in page at `app`, for the installation
    /// `client_id`, which a PIN is bound to.
    pub fn new(api: Url, app: Url, client_id: impl Into<String>) -> Result<Self, AccountError> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| AccountError::Unreachable(e.to_string()))?;
        Ok(Self {
            api,
            app,
            client_id: client_id.into(),
            http,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.api.as_str().trim_end_matches('/'))
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .header(reqwest::header::ACCEPT, "application/json")
            .header("X-Plex-Product", PRODUCT)
            .header("X-Plex-Client-Identifier", &self.client_id)
    }

    async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response, AccountError> {
        request
            .send()
            .await
            .map_err(|e| AccountError::Unreachable(e.to_string()))
    }

    /// A new PIN, with a short code (typeable at plex.tv/link).
    pub async fn begin(&self) -> Result<Pin, AccountError> {
        let response = Self::send(self.request(reqwest::Method::POST, "/api/v2/pins")).await?;
        if !response.status().is_success() {
            return Err(AccountError::Refused(response.status().to_string()));
        }
        let pin: PinReply = response
            .json()
            .await
            .map_err(|e| AccountError::Refused(format!("an unreadable PIN: {e}")))?;
        let mut auth = self.app.clone();
        auth.set_path("/auth");
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("clientID", &self.client_id)
            .append_pair("code", &pin.code)
            .append_pair("context[device][product]", PRODUCT)
            .finish();
        Ok(Pin {
            id: pin.id,
            auth_url: format!("{auth}#?{query}"),
            code: pin.code,
            expires_in: pin.expires_in,
        })
    }

    /// Whether PIN `id` is approved yet.
    pub async fn check(&self, id: u64) -> Result<PinState, AccountError> {
        let response =
            Self::send(self.request(reqwest::Method::GET, &format!("/api/v2/pins/{id}"))).await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(PinState::Expired);
        }
        if !response.status().is_success() {
            return Err(AccountError::Refused(response.status().to_string()));
        }
        let pin: PinReply = response
            .json()
            .await
            .map_err(|e| AccountError::Refused(format!("an unreadable PIN: {e}")))?;
        Ok(match pin.auth_token.filter(|t| !t.is_empty()) {
            Some(token) => PinState::Linked(token),
            None => PinState::Waiting,
        })
    }

    /// Who `token` signs in: the account's name, else its email.
    pub async fn account(&self, token: &str) -> Result<Option<String>, AccountError> {
        let response = Self::send(
            self.request(reqwest::Method::GET, "/api/v2/user")
                .header("X-Plex-Token", token),
        )
        .await?;
        if !response.status().is_success() {
            return Err(AccountError::Refused(response.status().to_string()));
        }
        let user: UserReply = response
            .json()
            .await
            .map_err(|e| AccountError::Refused(format!("an unreadable account: {e}")))?;
        Ok(user.username.or(user.email))
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    async fn plex_tv(server: &MockServer) -> PlexTv {
        PlexTv::new(
            server.uri().parse().unwrap(),
            "https://app.example".parse().unwrap(),
            "client-1",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_pin_is_begun_waited_on_and_linked() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/pins"))
            .and(header("X-Plex-Client-Identifier", "client-1"))
            .and(header("X-Plex-Product", "Spoolframe"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 77, "code": "4F7K", "expiresIn": 900, "authToken": null
            })))
            .mount(&server)
            .await;
        let tv = plex_tv(&server).await;
        let pin = tv.begin().await.unwrap();
        assert_eq!(pin.id, 77);
        assert_eq!(pin.code, "4F7K");
        assert_eq!(pin.expires_in, Some(900));
        assert_eq!(
            pin.auth_url,
            "https://app.example/auth#?clientID=client-1&code=4F7K&context%5Bdevice%5D%5Bproduct%5D=Spoolframe"
        );

        let waiting = Mock::given(method("GET"))
            .and(path("/api/v2/pins/77"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 77, "code": "4F7K", "authToken": null
            })))
            .up_to_n_times(1)
            .mount_as_scoped(&server)
            .await;
        assert_eq!(tv.check(77).await.unwrap(), PinState::Waiting);
        drop(waiting);
        Mock::given(method("GET"))
            .and(path("/api/v2/pins/77"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 77, "code": "4F7K", "authToken": "tok-123"
            })))
            .mount(&server)
            .await;
        let linked = tv.check(77).await.unwrap();
        assert_eq!(linked, PinState::Linked("tok-123".into()));
        assert!(!format!("{linked:?}").contains("tok-123"));
    }

    #[tokio::test]
    async fn a_forgotten_pin_is_expired_and_the_account_is_named() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v2/user"))
            .and(header("X-Plex-Token", "tok-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "username": "andrey", "email": "andrey@example.com"
            })))
            .mount(&server)
            .await;
        let tv = plex_tv(&server).await;
        assert_eq!(tv.check(78).await.unwrap(), PinState::Expired);
        assert_eq!(
            tv.account("tok-123").await.unwrap().as_deref(),
            Some("andrey")
        );
    }
}
