//! The server's document at its web address: how a device that has
//! only the address a person typed (`sf.example.org`) learns the key to
//! dial.
//!
//! Iroh dials by key, and a person cannot type 64 hex digits on a TV
//! remote. A server with `admin.public_url` set answers
//! `GET https://<its host>/.well-known/spoolframe` with
//! [`ServerDocument`] as JSON: what its invite links say of it before a
//! code is put in them ([`crate::invite::Invite`]), and no secret. A
//! server without `public_url` is not meant to be found from outside, and
//! answers 404.
//!
//! The document is fetched over HTTPS only: the key in it is what the
//! device then dials, and the user compares the fingerprint the QUIC
//! handshake proves with the one the server's web UI shows before they
//! save it as verified. Over plain HTTP anyone on the path could name
//! their own key.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::invite::{Invite, MAX_HINTS, is_relay_url};

/// Where the document is, at the root of the server's web origin.
pub const PATH: &str = "/.well-known/spoolframe";

/// The longest document a device reads, in bytes: one with
/// [`MAX_HINTS`] addresses and relays is a few KiB.
pub const MAX_DOCUMENT: usize = 64 * 1024;

/// How long a device or a proxy may keep the document, in seconds: its
/// addresses change when the server's network does.
pub const MAX_AGE_SECS: u32 = 300;

/// What a server says of itself at [`PATH`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerDocument {
    /// The server's `EndpointId`, hex: the key a device dials.
    pub endpoint_id: String,
    /// What the server calls itself.
    pub name: String,
    /// Addresses it may be reached at, `ip:port`.
    #[serde(default)]
    pub addrs: Vec<String>,
    /// The URLs of relays it may be reached through.
    #[serde(default)]
    pub relays: Vec<String>,
    /// The pkarr relay it publishes where it is to.
    #[serde(default)]
    pub lookup: Option<String>,
}

impl ServerDocument {
    /// The document of a server whose invite link (with no code) is
    /// `invite`: the same key, name, addresses, relays and lookup.
    pub fn of(invite: &Invite) -> Self {
        Self {
            endpoint_id: invite.server.clone(),
            name: invite.name.clone().unwrap_or_default(),
            addrs: invite
                .addrs
                .iter()
                .take(MAX_HINTS)
                .map(ToString::to_string)
                .collect(),
            relays: invite.relays.iter().take(MAX_HINTS).cloned().collect(),
            lookup: invite.lookup.clone(),
        }
    }

    /// The addresses that read as `ip:port`, each once, at most
    /// [`MAX_HINTS`]; the rest are left out.
    pub fn socket_addrs(&self) -> Vec<SocketAddr> {
        let mut addrs: Vec<SocketAddr> = Vec::new();
        for addr in self.addrs.iter().filter_map(|a| a.parse().ok()) {
            if !addrs.contains(&addr) {
                addrs.push(addr);
            }
            if addrs.len() == MAX_HINTS {
                break;
            }
        }
        addrs
    }

    /// The relays that are plain http(s) URLs, each once, at most
    /// [`MAX_HINTS`], as an invite link's are read.
    pub fn relay_urls(&self) -> Vec<String> {
        let mut relays: Vec<String> = Vec::new();
        for relay in self.relays.iter().filter(|r| is_relay_url(r)) {
            if !relays.contains(relay) {
                relays.push(relay.clone());
            }
            if relays.len() == MAX_HINTS {
                break;
            }
        }
        relays
    }

    /// The lookup, if it is a plain http(s) URL.
    pub fn lookup_url(&self) -> Option<String> {
        self.lookup.clone().filter(|url| is_relay_url(url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_document_says_what_the_invite_link_says() {
        let invite = Invite {
            server: "ab".repeat(32),
            name: Some("Home Media".into()),
            addrs: vec!["203.0.113.7:7420".parse().unwrap()],
            relays: vec!["https://relay.example.org/".into()],
            lookup: Some("https://dns.example.org/pkarr".into()),
            code: Some("QW7K2MFPXR4T".into()),
            ..Invite::default()
        };
        let json = serde_json::to_value(ServerDocument::of(&invite)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "endpoint_id": "ab".repeat(32),
                "name": "Home Media",
                "addrs": ["203.0.113.7:7420"],
                "relays": ["https://relay.example.org/"],
                "lookup": "https://dns.example.org/pkarr",
            })
        );
    }

    #[test]
    fn hints_are_read_as_an_invite_links_are() {
        let doc = ServerDocument {
            endpoint_id: String::new(),
            name: String::new(),
            addrs: (0..MAX_HINTS + 3)
                .map(|i| format!("10.0.0.{}:7420", i % 250))
                .chain(["not an address".into(), "10.0.0.0:7420".into()])
                .collect(),
            relays: vec![
                "https://relay.example.org/".into(),
                "https://relay.example.org/".into(),
                "ftp://relay.example.org/".into(),
                "http://relay.lan/".into(),
            ],
            lookup: Some("javascript:alert(1)".into()),
        };
        assert_eq!(doc.socket_addrs().len(), MAX_HINTS);
        assert_eq!(
            doc.relay_urls(),
            vec!["https://relay.example.org/", "http://relay.lan/"]
        );
        assert_eq!(doc.lookup_url(), None);
    }

    #[test]
    fn a_document_with_only_a_key_and_name_reads() {
        let doc: ServerDocument =
            serde_json::from_str(r#"{"endpoint_id":"ab","name":"x"}"#).unwrap();
        assert!(doc.addrs.is_empty() && doc.relays.is_empty() && doc.lookup.is_none());
    }
}
