//! Adding a server by the address a person typed (`sf.example.org`), as
//! on a TV with a remote, where neither a key nor an invite link can be
//! typed.
//!
//! Iroh dials by key, so the key comes from the document the server
//! publishes at its web address, `https://<host>/.well-known/spoolframe`
//! ([`spoolframe_protocol::well_known`]): its key, name, and the
//! addresses, relays and lookup an invite link would carry. The app's
//! flow:
//!
//! 1. [`Client::look_up_server`] with what was typed: the key, its
//!    fingerprint and the name, for "Add Home Media?". The document's
//!    hints join what the server is dialled with, as an invite link's do;
//!    nothing is saved and nothing is dialled.
//! 2. [`Client::connect`] with the key: the QUIC handshake proves the
//!    server answering holds it.
//! 3. [`Client::pin`], `verified` as the user says. Pinning saves the
//!    document's hints with the server, as saving an invite's server does
//!    ([`Client::save_invite_server`]): a document that names relays
//!    chooses them for it (`use_relays = Some(true)`), whatever the
//!    device's own relays setting. It also keeps the address it was
//!    looked up at, so that a later lookup there naming another key is
//!    [`SdkError::IdentityChanged`].
//!
//! The key came from the same address the user typed, so the fingerprint
//! proves only that the server answering QUIC holds the key the document
//! named, not that the document is the user's server's: they confirm it
//! against the fingerprint the server's web UI shows before saying it is
//! verified.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::Duration;

use iroh::EndpointId;
use spoolframe_protocol::fingerprint;
use spoolframe_protocol::invite::Invite;
use spoolframe_protocol::well_known::{MAX_DOCUMENT, PATH, ServerDocument};

use crate::client::{Client, parse_id};
use crate::servers::{self, SavedServer};
use crate::{SdkError, is_invite_link};

/// How long a lookup may take, all of it: DNS, TLS, the answer.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the connection to the address may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The most redirects followed, each to https only.
const MAX_REDIRECTS: usize = 3;

/// A server found at a typed address ([`Client::look_up_server`]):
/// what its document says. Nothing is saved.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ServerLookup {
    /// The server's key, hex: what [`Client::connect`] and
    /// [`Client::pin`] take.
    pub endpoint_id: String,
    /// What the server calls itself.
    pub name: String,
    /// The key's fingerprint, as an invite lookup shows it
    /// ([`crate::ServerEntry::fingerprint`]): what the user compares with
    /// the one the server's web UI shows.
    pub fingerprint: String,
    /// The addresses the document names: public ones only.
    pub addrs: Vec<SocketAddr>,
    /// The relays it names.
    pub relays: Vec<String>,
    /// The pkarr relay it names, where the server is looked up by its key.
    pub lookup: Option<String>,
}

/// What a person typed as the address of the server's document, and the
/// origin it is at: `(origin, document URL)`.
///
/// - Spaces around it are trimmed.
/// - No scheme is `https://`: `sf.example.org` is
///   `https://sf.example.org/.well-known/spoolframe`.
/// - `https://host`, and `https://host/any/path`, are that host's origin:
///   the path, query and fragment are dropped, as the document is always
///   at the origin's root.
/// - A port is kept (`host:8443`, `https://host:8443`); 443 is the
///   default, and dropped.
/// - The host is lowercased (`SF.Example.org` is `sf.example.org`).
/// - `http://` is [`SdkError::NotHttps`], unless `allow_http` (a test's).
/// - Nothing, another scheme, a user name or password, or anything that
///   is not a host is [`SdkError::InvalidInput`].
pub(crate) fn document_url(typed: &str, allow_http: bool) -> Result<(String, String), SdkError> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Err(SdkError::InvalidInput("type the server's address".into()));
    }
    if is_invite_link(typed) {
        return Err(SdkError::InvalidInput(
            "this is an invite link: open it as an invite".into(),
        ));
    }
    let with_scheme = match typed.split_once("://") {
        Some((scheme, _)) => match scheme.to_ascii_lowercase().as_str() {
            "https" => typed.to_owned(),
            "http" if allow_http => typed.to_owned(),
            "http" => return Err(SdkError::NotHttps),
            _ => {
                return Err(SdkError::InvalidInput(format!(
                    "{typed:?} is not a server's address: type its host, such as sf.example.org"
                )));
            }
        },
        None => format!("https://{typed}"),
    };
    let not_an_address = || {
        SdkError::InvalidInput(format!(
            "{typed:?} is not a server's address: type its host, such as sf.example.org"
        ))
    };
    let url = url::Url::parse(&with_scheme).map_err(|_| not_an_address())?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(not_an_address());
    }
    let host = match url.host() {
        Some(url::Host::Domain(name)) if is_host_name(name) => name.to_ascii_lowercase(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => format!("[{ip}]"),
        _ => return Err(not_an_address()),
    };
    let origin = match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    };
    let document = format!("{origin}{PATH}");
    Ok((origin, document))
}

/// Whether `name` reads as a DNS name: labels of letters, digits and
/// hyphens (or punycode), none empty.
fn is_host_name(name: &str) -> bool {
    let name = name.strip_suffix('.').unwrap_or(name);
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// The HTTP client a lookup is made with: TLS as `tls`, a
/// `rustls::ClientConfig` (the endpoint's own, webpki roots; the SDK
/// names no rustls of its own), https only unless `allow_http`.
pub(crate) fn http_client(
    tls: impl std::any::Any,
    allow_http: bool,
) -> Result<reqwest::Client, SdkError> {
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .https_only(!allow_http)
        .timeout(LOOKUP_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .user_agent(concat!("spoolframe-client-sdk/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| SdkError::Unavailable(format!("cannot look servers up: {e}")))
}

/// The document at `url`, of `origin`, fetched with `http`.
pub(crate) async fn fetch_document(
    http: &reqwest::Client,
    url: &str,
    origin: &str,
) -> Result<ServerDocument, SdkError> {
    let network = |e: reqwest::Error| {
        tracing::debug!(origin, error = %e, "a server's document did not come");
        if e.is_timeout() {
            SdkError::TimedOut(origin.to_owned())
        } else {
            SdkError::Unreachable(origin.to_owned())
        }
    };
    let mut response = http.get(url).send().await.map_err(network)?;
    let status = response.status();
    if status.is_server_error() {
        tracing::debug!(origin, %status, "a server's document did not come");
        return Err(SdkError::Unreachable(origin.to_owned()));
    }
    if !status.is_success() {
        return Err(SdkError::NotASpoolframeServer {
            address: origin.to_owned(),
        });
    }
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network)? {
        if body.len() + chunk.len() > MAX_DOCUMENT {
            return Err(SdkError::MalformedServerDocument {
                reason: format!("it is larger than {} KiB", MAX_DOCUMENT / 1024),
            });
        }
        body.extend_from_slice(&chunk);
    }
    read_document(&body, origin)
}

/// `body` as a server's document. Anything that is not a JSON object with
/// an `endpoint_id` is another site's page; one with it that does not
/// read is a document that cannot be used.
fn read_document(body: &[u8], origin: &str) -> Result<ServerDocument, SdkError> {
    let not_one = || SdkError::NotASpoolframeServer {
        address: origin.to_owned(),
    };
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| not_one())?;
    if value.get("endpoint_id").is_none() {
        return Err(not_one());
    }
    let doc: ServerDocument =
        serde_json::from_value(value).map_err(|e| SdkError::MalformedServerDocument {
            reason: e.to_string(),
        })?;
    if parse_id(&doc.endpoint_id).is_err() {
        return Err(SdkError::MalformedServerDocument {
            reason: "its endpoint_id is not a server key".into(),
        });
    }
    Ok(doc)
}

impl Client {
    /// Find the server at `address`, as a person typed it (`sf.example.org`,
    /// `https://sf.example.org:8443`; see the rules below): fetch its
    /// document over HTTPS, and say its key, fingerprint and name, for
    /// "Add this server?". Nothing is saved, and the server is not dialled.
    ///
    /// The document's addresses, relays and lookup join what the server is
    /// dialled with for as long as the app runs, as an invite link's do,
    /// so the app goes on with [`Client::connect`] (the handshake proves
    /// the key) and [`Client::pin`]. Nothing is pinned by this call.
    /// [`Client::pin`] saves the hints with the server: a document naming
    /// relays chooses them for it (`use_relays = Some(true)`), whatever
    /// [`crate::ClientOptions::relays`] says, as saving an invite's server
    /// does; and it keeps the address, so a later lookup there that names
    /// another key is refused.
    ///
    /// The address, before anything is sent: trimmed; with no scheme, it
    /// is `https://`; a path, query or fragment is dropped, as the
    /// document is always at the origin's root
    /// (`https://<host>[:port]/.well-known/spoolframe`); a port is kept;
    /// the host is lowercased.
    ///
    /// Refusals:
    /// - [`SdkError::InvalidInput`]: nothing typed, or not a host (another
    ///   scheme, a user name, an invite link); nothing is sent.
    /// - [`SdkError::NotHttps`]: an `http://` address; nothing is sent.
    /// - [`SdkError::Unreachable`] (no answer, DNS, TLS, the server's
    ///   5xx) and [`SdkError::TimedOut`]: the network's.
    /// - [`SdkError::NotASpoolframeServer`]: it answered with no document
    ///   (a 404, or another site's page): another site, or a server that
    ///   does not publish itself outside its home.
    /// - [`SdkError::MalformedServerDocument`]: a document that cannot be
    ///   used.
    /// - [`SdkError::IdentityChanged`]: a saved server was pinned after a
    ///   lookup at this same address, and the document now names another
    ///   key. Its hints are taken all the same, so
    ///   [`Client::trust_new_identity`]`(saved, now)` approves the new key
    ///   if the user says so; nothing approves it otherwise.
    pub async fn look_up_server(&self, address: &str) -> Result<ServerLookup, SdkError> {
        let allow_http = self.inner.http_lookups.load(Ordering::Relaxed);
        let (origin, url) = document_url(address, allow_http)?;
        let http = http_client(self.inner.endpoint.tls_config().clone(), allow_http)?;
        let doc = fetch_document(&http, &url, &origin).await?;
        let id: EndpointId =
            parse_id(&doc.endpoint_id).map_err(|_| SdkError::MalformedServerDocument {
                reason: "its endpoint_id is not a server key".into(),
            })?;
        let invite = Invite {
            server: id.to_string(),
            name: Some(doc.name.clone()),
            addrs: doc.socket_addrs(),
            relays: doc.relay_urls(),
            lookup: doc.lookup_url(),
            ..Invite::default()
        };
        self.take_hints(&invite, id);
        self.inner
            .looked_up
            .lock()
            .insert(id, (origin.clone(), invite.clone()));
        let saved = servers::load(self.inner.storage.as_ref()).await?;
        if let Some(old) = key_changed(&saved, &origin, id) {
            tracing::warn!(
                origin,
                saved = old.endpoint_id,
                now = %id,
                "the address a saved server was looked up at names another key"
            );
            return Err(SdkError::IdentityChanged {
                saved: old.endpoint_id.clone(),
                now: id.to_string(),
                server_id: old.server_id.clone(),
            });
        }
        Ok(ServerLookup {
            endpoint_id: id.to_string(),
            name: doc.name,
            fingerprint: fingerprint::fingerprint(id.as_bytes()),
            addrs: invite.addrs,
            relays: invite.relays,
            lookup: invite.lookup,
        })
    }

    /// Let [`Client::look_up_server`] fetch `http://` addresses: a test
    /// server has no certificate. Never in an app.
    #[cfg(any(test, feature = "test-http-lookup"))]
    #[doc(hidden)]
    pub fn allow_http_lookups(&self) {
        self.inner.http_lookups.store(true, Ordering::Relaxed);
    }

    /// A lookup's hints for `id` this run, saved with `server` as an
    /// invite's are, with the address it was looked up at.
    pub(crate) fn apply_looked_up(&self, id: EndpointId, server: &mut SavedServer) {
        let Some((origin, invite)) = self.inner.looked_up.lock().get(&id).cloned() else {
            return;
        };
        crate::invites::hint(server, &invite, servers::now());
        server.looked_up_at = Some(origin);
    }
}

/// The saved server last looked up at `origin`, if `id` is not saved and
/// it is under another key.
fn key_changed<'a>(
    saved: &'a [SavedServer],
    origin: &str,
    id: EndpointId,
) -> Option<&'a SavedServer> {
    let id = id.to_string();
    if saved.iter().any(|s| s.endpoint_id == id) {
        return None;
    }
    saved
        .iter()
        .find(|s| s.looked_up_at.as_deref() == Some(origin))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(typed: &str) -> Result<String, SdkError> {
        document_url(typed, false).map(|(_, url)| url)
    }

    #[test]
    fn a_bare_host_is_its_https_origin() {
        assert_eq!(
            url("sf.coldsteel.casa").unwrap(),
            "https://sf.coldsteel.casa/.well-known/spoolframe"
        );
    }

    #[test]
    fn spaces_around_it_are_trimmed() {
        assert_eq!(
            url("  sf.coldsteel.casa \n").unwrap(),
            "https://sf.coldsteel.casa/.well-known/spoolframe"
        );
    }

    #[test]
    fn a_path_query_and_fragment_are_dropped() {
        for typed in [
            "https://sf.coldsteel.casa",
            "https://sf.coldsteel.casa/",
            "https://sf.coldsteel.casa/any/path?x=1#y",
            "sf.coldsteel.casa/web/",
        ] {
            assert_eq!(
                url(typed).unwrap(),
                "https://sf.coldsteel.casa/.well-known/spoolframe",
                "{typed}"
            );
        }
    }

    #[test]
    fn a_port_is_kept_and_443_dropped() {
        assert_eq!(
            url("sf.coldsteel.casa:8443").unwrap(),
            "https://sf.coldsteel.casa:8443/.well-known/spoolframe"
        );
        assert_eq!(
            url("https://sf.coldsteel.casa:8443/x").unwrap(),
            "https://sf.coldsteel.casa:8443/.well-known/spoolframe"
        );
        assert_eq!(
            url("https://sf.coldsteel.casa:443").unwrap(),
            "https://sf.coldsteel.casa/.well-known/spoolframe"
        );
    }

    #[test]
    fn the_host_and_scheme_are_lowercased() {
        assert_eq!(
            document_url("HTTPS://SF.Coldsteel.CASA", false).unwrap(),
            (
                "https://sf.coldsteel.casa".to_owned(),
                "https://sf.coldsteel.casa/.well-known/spoolframe".to_owned()
            )
        );
    }

    #[test]
    fn addresses_by_ip_are_hosts_too() {
        assert_eq!(
            url("203.0.113.7:8443").unwrap(),
            "https://203.0.113.7:8443/.well-known/spoolframe"
        );
        assert_eq!(
            url("https://[2001:db8::7]/").unwrap(),
            "https://[2001:db8::7]/.well-known/spoolframe"
        );
    }

    #[test]
    fn http_is_refused_before_anything_is_sent() {
        assert_eq!(url("http://sf.coldsteel.casa"), Err(SdkError::NotHttps));
        assert_eq!(url("HTTP://sf.coldsteel.casa"), Err(SdkError::NotHttps));
    }

    #[test]
    fn what_is_not_a_host_is_invalid_input() {
        for typed in [
            "",
            "   ",
            "ftp://sf.coldsteel.casa",
            "user:pw@sf.coldsteel.casa",
            "two words",
            "sf.coldsteel.casa:notaport",
            "spoolframe://invite?server=ab",
            "https://",
            "a..b",
        ] {
            assert!(
                matches!(url(typed), Err(SdkError::InvalidInput(_))),
                "{typed:?}: {:?}",
                url(typed)
            );
        }
    }

    #[test]
    fn a_page_that_is_not_a_document_is_not_a_spoolframe_server() {
        for body in [&b"<html></html>"[..], b"[]", b"{\"name\":\"x\"}"] {
            assert_eq!(
                read_document(body, "https://h"),
                Err(SdkError::NotASpoolframeServer {
                    address: "https://h".into()
                })
            );
        }
    }

    #[test]
    fn a_document_with_a_bad_key_or_field_is_malformed() {
        for body in [
            &br#"{"endpoint_id":"nope","name":"x"}"#[..],
            br#"{"endpoint_id":7,"name":"x"}"#,
        ] {
            assert!(
                matches!(
                    read_document(body, "https://h"),
                    Err(SdkError::MalformedServerDocument { .. })
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// One HTTP answer, `response`, to the first request on a loopback
    /// port; the origin to ask.
    async fn answering(response: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            let _ = socket.write_all(&response).await;
            let _ = socket.shutdown().await;
        });
        format!("http://{addr}")
    }

    fn test_client() -> reqwest::Client {
        let tls = iroh::tls::CaTlsConfig::default()
            .client_config(iroh::tls::default_provider())
            .unwrap();
        http_client(tls, true).unwrap()
    }

    async fn fetched(response: Vec<u8>) -> Result<ServerDocument, SdkError> {
        let origin = answering(response).await;
        fetch_document(&test_client(), &format!("{origin}{PATH}"), &origin).await
    }

    fn http(status: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[tokio::test]
    async fn a_404_is_not_a_spoolframe_server() {
        assert!(matches!(
            fetched(http("404 Not Found", b"")).await,
            Err(SdkError::NotASpoolframeServer { .. })
        ));
    }

    #[tokio::test]
    async fn a_document_past_the_cap_is_malformed() {
        let body = vec![b' '; MAX_DOCUMENT + 1];
        assert!(matches!(
            fetched(http("200 OK", &body)).await,
            Err(SdkError::MalformedServerDocument { .. })
        ));
    }

    #[tokio::test]
    async fn a_good_document_reads() {
        let id = iroh::SecretKey::generate().public().to_string();
        let body = format!(
            r#"{{"endpoint_id":"{id}","name":"Home Media","addrs":["198.51.100.1:7420"]}}"#
        );
        let doc = fetched(http("200 OK", body.as_bytes())).await.unwrap();
        assert_eq!((doc.endpoint_id, doc.name), (id, "Home Media".to_owned()));
    }

    #[tokio::test]
    async fn no_answer_is_unreachable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        assert_eq!(
            fetch_document(&test_client(), &format!("{origin}{PATH}"), &origin).await,
            Err(SdkError::Unreachable(origin))
        );
    }

    #[test]
    fn only_another_key_at_the_same_address_is_a_change() {
        let saved_id = iroh::SecretKey::generate().public();
        let new_id = iroh::SecretKey::generate().public();
        let mut server = SavedServer::pinned_now(
            "Home Media".into(),
            saved_id.to_string(),
            "sid".into(),
            Vec::new(),
            true,
        );
        server.looked_up_at = Some("https://sf.example.org".into());
        let saved = vec![server];
        assert!(key_changed(&saved, "https://sf.example.org", new_id).is_some());
        assert!(key_changed(&saved, "https://sf.example.org", saved_id).is_none());
        assert!(key_changed(&saved, "https://other.example.org", new_id).is_none());
    }
}
