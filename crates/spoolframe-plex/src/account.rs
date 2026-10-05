//! Sign in with Plex (P3-10): plex.tv's PIN flow, which gets a Plex
//! account's token without anyone typing it. The server asks plex.tv for a
//! strong PIN, the browser opens Plex's sign-in page with it, and the
//! server polls the PIN until plex.tv hands it the token.
//!
//! The PIN is a strong one: app.plex.tv/auth refuses a short one with a
//! "Security Alert: another device is attempting to sign in". A strong
//! PIN's code is long and only ever travels inside the sign-in page's
//! address; nobody types it at plex.tv/link.
//!
//! A linked account's token also lists the Plex servers it reaches, each
//! with a token of its own for that server (P3-12).
//!
//! The only part of this crate that reaches the Internet, and only while
//! someone signs in or a linked account's servers are looked up: the
//! catalog itself talks to the PMS on the LAN alone (ARCHITECTURE §32).
//! Both base URLs are given, so tests use a stand-in.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use url::Url;

/// plex.tv's API.
pub const PLEX_TV: &str = "https://plex.tv";
/// Where a person approves a PIN.
pub const PLEX_APP: &str = "https://app.plex.tv";

const PRODUCT: &str = "Spoolframe";
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// What holds the token: a server, not the browser that signs in.
const DEVICE: &str = "Server";
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
    /// The strong PIN's code, already in `auth_url`: long, and not for
    /// typing at plex.tv/link.
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

/// Who a token signs in, as plex.tv says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct PlexAccount {
    pub username: Option<String>,
    pub email: Option<String>,
}

/// A Plex Media Server an account reaches: its own, or one shared with it.
#[derive(Clone, PartialEq, Eq)]
pub struct PlexResource {
    pub name: String,
    /// The PMS's machine identifier (`/identity`).
    pub machine_id: String,
    /// The account's own server, rather than one shared with it.
    pub owned: bool,
    /// The token this account uses on this server: plex.tv hands out one
    /// per server, which is how Plex's own apps reach a shared server.
    /// Never shown or logged.
    pub access_token: Option<String>,
    /// Where it can be reached, the LAN's first.
    pub connections: Vec<PlexConnection>,
}

impl fmt::Debug for PlexResource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlexResource")
            .field("name", &self.name)
            .field("machine_id", &self.machine_id)
            .field("owned", &self.owned)
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("connections", &self.connections)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlexConnection {
    pub uri: String,
    /// The address and port `uri` stands for, as plex.tv says them: a
    /// plex.direct name's LAN address, which is how the server is found
    /// on the LAN.
    pub address: Option<String>,
    pub port: Option<u16>,
    /// On the server's own network.
    pub local: bool,
    /// Through Plex's relay: slow, and never what Spoolframe reads from.
    pub relay: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResourceReply {
    name: String,
    #[serde(default)]
    provides: String,
    client_identifier: String,
    #[serde(default)]
    owned: bool,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    connections: Vec<ConnectionReply>,
}

#[derive(Deserialize)]
struct ConnectionReply {
    uri: String,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    local: bool,
    #[serde(default)]
    relay: bool,
}

/// The server's operating system, as Plex's own apps name theirs.
fn platform() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "macOS",
        "windows" => "Windows",
        "freebsd" => "FreeBSD",
        other => other,
    }
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
            .header("X-Plex-Version", VERSION)
            .header("X-Plex-Platform", platform())
            .header("X-Plex-Device", DEVICE)
            .header("X-Plex-Client-Identifier", &self.client_id)
    }

    async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response, AccountError> {
        request
            .send()
            .await
            .map_err(|e| AccountError::Unreachable(e.to_string()))
    }

    /// A new strong PIN and Plex's sign-in page for it. `forward`: the
    /// page Plex's sign-in goes on to once it is done (`forwardUrl`);
    /// without one, it ends on "We were unable to complete this request"
    /// although the PIN was approved. Polling the PIN ([`Self::check`])
    /// is what links; the page there only tells the person.
    ///
    /// The page's address carries `clientID`, `code`, the device context
    /// and `forwardUrl`. The context is the one every request here sends as
    /// `X-Plex-*` headers, `clientID` the `X-Plex-Client-Identifier`: plex.tv
    /// binds the PIN to that installation, and names it by that context in
    /// its device list and security notices.
    pub async fn begin(&self, forward: Option<&str>) -> Result<Pin, AccountError> {
        let response = Self::send(
            self.request(reqwest::Method::POST, "/api/v2/pins")
                .query(&[("strong", "true")]),
        )
        .await?;
        if !response.status().is_success() {
            return Err(AccountError::Refused(response.status().to_string()));
        }
        let pin: PinReply = response
            .json()
            .await
            .map_err(|e| AccountError::Refused(format!("an unreadable PIN: {e}")))?;
        let mut auth = self.app.clone();
        auth.set_path("/auth");
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("clientID", &self.client_id)
            .append_pair("code", &pin.code)
            .append_pair("context[device][product]", PRODUCT)
            .append_pair("context[device][version]", VERSION)
            .append_pair("context[device][platform]", platform())
            .append_pair("context[device][device]", DEVICE);
        if let Some(forward) = forward {
            query.append_pair("forwardUrl", forward);
        }
        let query = query.finish();
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
        let user = self.user(token).await?;
        Ok(user.username.or(user.email))
    }

    /// Who `token` signs in: the account's name and email.
    pub async fn user(&self, token: &str) -> Result<PlexAccount, AccountError> {
        let response = Self::send(
            self.request(reqwest::Method::GET, "/api/v2/user")
                .header("X-Plex-Token", token),
        )
        .await?;
        if !response.status().is_success() {
            return Err(AccountError::Refused(response.status().to_string()));
        }
        response
            .json()
            .await
            .map_err(|e| AccountError::Refused(format!("an unreadable account: {e}")))
    }

    /// The Plex Media Servers `token`'s account reaches, its own and those
    /// shared with it, each with the token it uses there.
    pub async fn servers(&self, token: &str) -> Result<Vec<PlexResource>, AccountError> {
        let response = Self::send(
            self.request(reqwest::Method::GET, "/api/v2/resources")
                .query(&[("includeHttps", "1"), ("includeRelay", "1")])
                .header("X-Plex-Token", token),
        )
        .await?;
        if !response.status().is_success() {
            return Err(AccountError::Refused(response.status().to_string()));
        }
        let resources: Vec<ResourceReply> = response
            .json()
            .await
            .map_err(|e| AccountError::Refused(format!("an unreadable server list: {e}")))?;
        Ok(resources
            .into_iter()
            .filter(|r| r.provides.split(',').any(|p| p.trim() == "server"))
            .map(|r| {
                let mut connections: Vec<PlexConnection> = r
                    .connections
                    .into_iter()
                    .map(|c| PlexConnection {
                        uri: c.uri,
                        address: c.address,
                        port: c.port,
                        local: c.local,
                        relay: c.relay,
                    })
                    .collect();
                connections.sort_by_key(|c| (c.relay, !c.local));
                PlexResource {
                    name: r.name,
                    machine_id: r.client_identifier,
                    owned: r.owned,
                    access_token: r.access_token.filter(|t| !t.is_empty()),
                    connections,
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path, query_param};
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
            // app.plex.tv/auth takes only a strong PIN; a short one is
            // refused there with a "Security Alert".
            .and(query_param("strong", "true"))
            .and(header("X-Plex-Client-Identifier", "client-1"))
            .and(header("X-Plex-Product", "Spoolframe"))
            .and(header("X-Plex-Version", VERSION))
            .and(header("X-Plex-Platform", platform()))
            .and(header("X-Plex-Device", "Server"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 77, "code": "q3n8vbx2kd5w7rjh0tyc4mzpf", "expiresIn": 900, "authToken": null
            })))
            .mount(&server)
            .await;
        let tv = plex_tv(&server).await;
        let pin = tv
            .begin(Some("http://media.lan:8090/auth/plex"))
            .await
            .unwrap();
        assert_eq!(pin.id, 77);
        assert_eq!(pin.code, "q3n8vbx2kd5w7rjh0tyc4mzpf");
        assert_eq!(pin.expires_in, Some(900));
        let (page, fragment) = pin.auth_url.split_once("#?").unwrap();
        assert_eq!(page, "https://app.example/auth");
        let params: Vec<(String, String)> = url::form_urlencoded::parse(fragment.as_bytes())
            .into_owned()
            .collect();
        let expected: Vec<(String, String)> = [
            ("clientID", "client-1"),
            ("code", "q3n8vbx2kd5w7rjh0tyc4mzpf"),
            ("context[device][product]", "Spoolframe"),
            ("context[device][version]", VERSION),
            ("context[device][platform]", platform()),
            ("context[device][device]", "Server"),
            ("forwardUrl", "http://media.lan:8090/auth/plex"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        assert_eq!(params, expected);

        let waiting = Mock::given(method("GET"))
            .and(path("/api/v2/pins/77"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 77, "code": "q3n8vbx2kd5w7rjh0tyc4mzpf", "authToken": null
            })))
            .up_to_n_times(1)
            .mount_as_scoped(&server)
            .await;
        assert_eq!(tv.check(77).await.unwrap(), PinState::Waiting);
        drop(waiting);
        Mock::given(method("GET"))
            .and(path("/api/v2/pins/77"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 77, "code": "q3n8vbx2kd5w7rjh0tyc4mzpf", "authToken": "tok-123"
            })))
            .mount(&server)
            .await;
        let linked = tv.check(77).await.unwrap();
        assert_eq!(linked, PinState::Linked("tok-123".into()));
        assert!(!format!("{linked:?}").contains("tok-123"));
    }

    #[tokio::test]
    async fn without_a_page_to_go_back_to_the_sign_in_has_no_forward_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/pins"))
            .and(query_param("strong", "true"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 9, "code": "q3n8vbx2kd5w7rjh0tyc4mzpf", "authToken": null
            })))
            .mount(&server)
            .await;
        let pin = plex_tv(&server).await.begin(None).await.unwrap();
        assert!(!pin.auth_url.contains("forwardUrl"), "{}", pin.auth_url);
    }

    #[tokio::test]
    async fn an_account_lists_its_servers_with_a_token_for_each_and_the_lan_first() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v2/resources"))
            .and(header("X-Plex-Token", "tok-alice"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "name": "Alice's phone", "provides": "client,player",
                  "clientIdentifier": "phone", "owned": true, "connections": [] },
                { "name": "Tower", "provides": "server", "clientIdentifier": "tower-machine",
                  "owned": false, "accessToken": "alice-on-tower", "connections": [
                    { "uri": "https://relay.plex.direct:8443", "local": false, "relay": true },
                    { "uri": "https://1-2-3-4.tower.plex.direct:32400", "local": false, "relay": false },
                    { "uri": "http://10.0.0.5:32400", "local": true, "relay": false }
                ] }
            ])))
            .mount(&server)
            .await;
        let servers = plex_tv(&server).await.servers("tok-alice").await.unwrap();
        assert_eq!(servers.len(), 1, "{servers:?}");
        let tower = &servers[0];
        assert_eq!(tower.machine_id, "tower-machine");
        assert!(!tower.owned);
        assert_eq!(tower.access_token.as_deref(), Some("alice-on-tower"));
        assert_eq!(tower.connections[0].uri, "http://10.0.0.5:32400");
        assert!(tower.connections[2].relay);
        assert!(!format!("{tower:?}").contains("alice-on-tower"));
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
        assert_eq!(
            tv.user("tok-123").await.unwrap().email.as_deref(),
            Some("andrey@example.com")
        );
    }
}
