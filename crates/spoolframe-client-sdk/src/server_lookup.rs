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
use crate::{LookupFailure, SdkError, is_invite_link};

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
    builder(allow_http)
        .tls_backend_preconfigured(tls)
        .build()
        .map_err(|e| SdkError::Unavailable(format!("cannot look servers up: {e}")))
}

/// [`http_client`] but its TLS.
fn builder(allow_http: bool) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .https_only(!allow_http)
        .timeout(LOOKUP_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .dns_resolver(std::sync::Arc::new(Resolver))
        .user_agent(concat!("spoolframe-client-sdk/", env!("CARGO_PKG_VERSION")))
}

/// The system's resolver (getaddrinfo, as reqwest's own), failing with
/// [`NameNotFound`] only when the resolver says definitely that the name
/// does not exist, so a lookup can tell a typo from a network without
/// DNS. Any other failure (`EAI_AGAIN`, no DNS server, a resolver error)
/// is [`NoDns`], which reads as no answer, worth trying again.
struct Resolver;

/// The name a lookup asked for does not exist: the resolver said so
/// (`EAI_NONAME`, from an NXDOMAIN), or gave no address for it.
#[derive(Debug, thiserror::Error)]
#[error("{name} does not resolve")]
struct NameNotFound {
    name: String,
}

/// The name could not be resolved now, for `code` (a `getaddrinfo`
/// error): not a sign that it does not exist.
#[derive(Debug, thiserror::Error)]
#[error("{name} could not be resolved now (getaddrinfo {code})")]
struct NoDns {
    name: String,
    code: i32,
}

/// What `getaddrinfo` said of a name.
#[derive(Debug, PartialEq, Eq)]
enum Resolved {
    Found(Vec<SocketAddr>),
    /// It definitely does not exist, or has no address.
    NotFound,
    /// It could not be resolved now: this `getaddrinfo` error.
    Failed(i32),
}

impl Resolved {
    /// What a `getaddrinfo` answer means: `code` its return, `found` the
    /// addresses it gave.
    fn of(code: i32, found: Vec<SocketAddr>) -> Self {
        match code {
            0 if found.is_empty() => Self::NotFound,
            0 => Self::Found(found),
            libc::EAI_NONAME => Self::NotFound,
            other => Self::Failed(other),
        }
    }
}

/// `name`'s addresses, by the system's `getaddrinfo`, blocking.
fn getaddrinfo(name: &str) -> Resolved {
    let Ok(host) = std::ffi::CString::new(name) else {
        // A name with a NUL in it names nothing.
        return Resolved::NotFound;
    };
    // SAFETY: an all-zero addrinfo is the "no hints" value; only the
    // socket type is set.
    let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
    hints.ai_socktype = libc::SOCK_STREAM;
    let mut list: *mut libc::addrinfo = std::ptr::null_mut();
    // SAFETY: `host` is NUL-terminated and outlives the call, `hints` is
    // a valid addrinfo, and `list` is written only on success.
    let code = unsafe { libc::getaddrinfo(host.as_ptr(), std::ptr::null(), &hints, &mut list) };
    let mut found = Vec::new();
    if code == 0 {
        let mut at = list;
        while !at.is_null() {
            // SAFETY: each node of the list getaddrinfo gave is valid until
            // freeaddrinfo below.
            let entry = unsafe { &*at };
            if !entry.ai_addr.is_null() {
                match entry.ai_family {
                    libc::AF_INET => {
                        // SAFETY: an AF_INET entry's address is a sockaddr_in.
                        let v4 = unsafe { &*(entry.ai_addr as *const libc::sockaddr_in) };
                        let ip = std::net::Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr));
                        found.push(SocketAddr::from((ip, 0)));
                    }
                    libc::AF_INET6 => {
                        // SAFETY: an AF_INET6 entry's address is a sockaddr_in6.
                        let v6 = unsafe { &*(entry.ai_addr as *const libc::sockaddr_in6) };
                        let ip = std::net::Ipv6Addr::from(v6.sin6_addr.s6_addr);
                        found.push(SocketAddr::V6(std::net::SocketAddrV6::new(
                            ip,
                            0,
                            0,
                            v6.sin6_scope_id,
                        )));
                    }
                    _ => {}
                }
            }
            at = entry.ai_next;
        }
        // SAFETY: `list` came from a successful getaddrinfo, freed once.
        unsafe { libc::freeaddrinfo(list) };
    }
    Resolved::of(code, found)
}

impl reqwest::dns::Resolve for Resolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let name = name.as_str().to_owned();
        Box::pin(async move {
            let asked = name.clone();
            let resolved = tokio::task::spawn_blocking(move || getaddrinfo(&asked))
                .await
                .unwrap_or(Resolved::Failed(libc::EAI_FAIL));
            // reqwest sets the port of what this answers.
            match resolved {
                Resolved::Found(found) => Ok(Box::new(found.into_iter()) as reqwest::dns::Addrs),
                Resolved::NotFound => Err(NameNotFound { name }.into()),
                Resolved::Failed(code) => Err(NoDns { name, code }.into()),
            }
        })
    }
}

/// Why `e`, a lookup's request or body read, failed: by what its causes
/// are, never by their text.
fn failure_of(e: &reqwest::Error) -> LookupFailure {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(error) = cause {
        if error.is::<NameNotFound>() {
            return LookupFailure::NameNotFound;
        }
        if error.is::<rustls::Error>() {
            return LookupFailure::Tls;
        }
        // hyper-rustls hands rustls's error on inside an io::Error (inside
        // another, by hyper-util), and an io::Error's `source()` is what
        // it wraps's source, skipping it: the wrapped error is the next.
        cause = match error.downcast_ref::<std::io::Error>() {
            Some(io) => io.get_ref().map(|inner| inner as _),
            None => error.source(),
        };
    }
    if e.is_timeout() {
        LookupFailure::TimedOut
    } else {
        LookupFailure::NoAnswer
    }
}

/// The document at `url`, of `origin`, fetched with `http`.
pub(crate) async fn fetch_document(
    http: &reqwest::Client,
    url: &str,
    origin: &str,
) -> Result<ServerDocument, SdkError> {
    let failed = |reason| SdkError::ServerLookupFailed {
        address: origin.to_owned(),
        reason,
    };
    let network = |e: reqwest::Error| {
        let reason = failure_of(&e);
        tracing::debug!(origin, error = %e, ?reason, "a server's document did not come");
        failed(reason)
    };
    let mut response = http.get(url).send().await.map_err(network)?;
    let status = response.status();
    if status.is_server_error() {
        // A proxy answering for a server that is down.
        tracing::debug!(origin, %status, "a server's document did not come");
        return Err(failed(LookupFailure::NoAnswer));
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
    /// - [`SdkError::ServerLookupFailed`]: no answer to read, with its
    ///   [`crate::LookupFailure`]: the name does not resolve
    ///   (`NameNotFound`), nothing answers or the server behind a proxy is
    ///   down, a 5xx (`NoAnswer`), not in time (`TimedOut`), or the secure
    ///   connection failed (`Tls`).
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

    /// What `origin`'s lookup failed with, by `client`; `None` when it
    /// did not fail so.
    async fn failure_with(client: &reqwest::Client, origin: &str) -> Option<LookupFailure> {
        match fetch_document(client, &format!("{origin}{PATH}"), origin).await {
            Err(SdkError::ServerLookupFailed { address, reason }) => {
                assert_eq!(address, origin);
                Some(reason)
            }
            other => panic!("{origin}: {other:?}"),
        }
    }

    async fn failure(origin: &str) -> Option<LookupFailure> {
        failure_with(&test_client(), origin).await
    }

    /// Only a definite "no such name" is not found: a resolver that
    /// cannot answer now (no DNS server, a temporary failure) is not a
    /// typo, and reads as no answer.
    #[test]
    fn only_a_definite_no_such_name_is_not_found() {
        let at = SocketAddr::from(([192, 0, 2, 1], 0));
        assert_eq!(Resolved::of(0, vec![at]), Resolved::Found(vec![at]));
        assert_eq!(Resolved::of(0, Vec::new()), Resolved::NotFound);
        assert_eq!(
            Resolved::of(libc::EAI_NONAME, Vec::new()),
            Resolved::NotFound
        );
        for code in [
            libc::EAI_AGAIN,
            libc::EAI_FAIL,
            libc::EAI_SYSTEM,
            libc::EAI_MEMORY,
        ] {
            assert_eq!(Resolved::of(code, Vec::new()), Resolved::Failed(code));
        }
    }

    /// A name that cannot be resolved now fails as no answer, which is
    /// worth trying again; one that does not exist, as not found.
    #[test]
    fn a_resolver_failure_is_no_answer_and_retryable() {
        let failed = |reason| SdkError::ServerLookupFailed {
            address: "https://sf.example.org".into(),
            reason,
        };
        assert!(failed(LookupFailure::NoAnswer).is_transport());
        assert!(failed(LookupFailure::TimedOut).is_transport());
        assert!(!failed(LookupFailure::NameNotFound).is_transport());
        assert_eq!(
            failed(LookupFailure::NoAnswer).kind(),
            crate::ErrorKind::Transport
        );
        let no_dns: Box<dyn std::error::Error + Send + Sync> = NoDns {
            name: "sf.example.org".into(),
            code: libc::EAI_AGAIN,
        }
        .into();
        assert!(!no_dns.is::<NameNotFound>());
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_is_not_found() {
        // `.invalid` never resolves (RFC 6761).
        assert_eq!(
            failure("https://spoolframe-lookup-test.invalid").await,
            Some(LookupFailure::NameNotFound)
        );
    }

    #[tokio::test]
    async fn a_refused_port_is_no_answer() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        assert_eq!(failure(&origin).await, Some(LookupFailure::NoAnswer));
    }

    #[tokio::test]
    async fn a_server_error_is_no_answer() {
        let origin = answering(http("502 Bad Gateway", b"")).await;
        assert_eq!(failure(&origin).await, Some(LookupFailure::NoAnswer));
    }

    #[tokio::test]
    async fn plain_http_where_tls_was_asked_for_is_a_tls_failure() {
        let origin = answering(http("200 OK", b"{}"))
            .await
            .replace("http://", "https://");
        assert_eq!(failure(&origin).await, Some(LookupFailure::Tls));
    }

    #[tokio::test]
    async fn a_certificate_no_root_signed_is_a_tls_failure() {
        let origin = self_signed().await;
        assert_eq!(failure(&origin).await, Some(LookupFailure::Tls));
    }

    #[tokio::test]
    async fn a_server_that_accepts_and_never_answers_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            // Held open, never read or written.
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let tls = iroh::tls::CaTlsConfig::default()
            .client_config(iroh::tls::default_provider())
            .unwrap();
        let client = builder(true)
            .timeout(Duration::from_millis(300))
            .tls_backend_preconfigured(tls)
            .build()
            .unwrap();
        assert_eq!(
            failure_with(&client, &origin).await,
            Some(LookupFailure::TimedOut)
        );
    }

    /// An HTTPS server on loopback whose certificate no root signed, as a
    /// server at home with its own certificate has; the origin to ask.
    async fn self_signed() -> String {
        use std::sync::Arc;
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key =
            rustls::pki_types::PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certified.cert.der().clone()], key)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            // The client refuses the certificate: the handshake fails.
            let _ = acceptor.accept(socket).await;
        });
        format!("https://localhost:{port}")
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
