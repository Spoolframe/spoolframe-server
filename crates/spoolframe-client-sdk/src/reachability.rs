//! How this device stands with one server, for an app's "can't be
//! reached" details ([`crate::Client::reachability`]): what it knows of
//! the server, what its lookups last said, how its last dial went, and the
//! paths of the connection open now.
//!
//! Iroh dials every address and relay it knows at once and gives back one
//! outcome. It does not say how each path of a failed dial fared, so
//! neither does this: a dial's addresses are the ones iroh knew of then,
//! and its stage is where the dial as a whole stopped. What an address
//! lookup found is recorded here as each one answers, as iroh does not
//! keep it.

use std::collections::HashMap;
use std::net::SocketAddr;

use iroh::address_lookup::Item;
use iroh::{EndpointId, TransportAddr};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::link::ConnectionPath;
use crate::servers::{self, AddrSource};

/// The most lookup answers kept, across servers and lookups: the newest.
pub(crate) const MAX_LOOKUP_RESULTS: usize = 64;
/// The most servers whose last dial is kept: the newest.
pub(crate) const MAX_DIALS: usize = 32;
/// The most addresses kept of one lookup answer or one dial.
pub(crate) const MAX_LISTED: usize = 16;

/// One server, as this device can reach it now. Unix seconds throughout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reachability {
    pub endpoint_id: String,
    /// The name it last answered with, or was heard as on the LAN.
    pub name: Option<String>,
    /// Pinned on this device.
    pub saved: bool,
    /// The addresses kept for it, newest first.
    pub addrs: Vec<SavedAddr>,
    /// What its last `ServerAddresses` event listed, and when it came.
    pub advertised_addrs: Vec<String>,
    pub advertised_at: Option<u64>,
    /// The relays kept for it, and whether they are the user's list.
    pub relays: Vec<String>,
    pub relays_edited: bool,
    /// The pkarr relay it publishes to, as it or an invite said.
    pub lookup: Option<String>,
    /// Its relay setting as saved: `None` follows the device's.
    pub use_relays: Option<bool>,
    /// Whether it is dialled through relays now: its setting, else the
    /// device's, as the dialler applies it (an invite acted on in this run
    /// turns them on).
    pub relays_effective: bool,
    /// Where it answers on the LAN now (mDNS); empty when not heard.
    pub heard_on_lan: Vec<String>,
    /// Addresses typed in or from an invite in this run.
    pub hinted_addrs: Vec<String>,
    /// Relays an invite named in this run.
    pub invite_relays: Vec<String>,
    /// The address lookups in use for every server: `mdns`, and each pkarr
    /// relay's URL.
    pub lookups: Vec<String>,
    /// What each lookup last answered about this server.
    pub lookup_results: Vec<LookupResult>,
    /// The last dial in this run, if any.
    pub last_dial: Option<DialReport>,
    /// When it last answered a dial as itself, in any run.
    pub last_connected_at: Option<u64>,
    /// The paths of the connection open now; empty without one.
    pub live: Vec<LivePath>,
}

/// An address kept for a server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedAddr {
    pub addr: String,
    /// `true`: a connection reached it there; `false`: only the server
    /// said so.
    pub reached: bool,
    pub last_seen: u64,
}

/// What one lookup answered about one server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupResult {
    /// `mdns`, or the pkarr relay's URL.
    pub lookup: String,
    pub at: u64,
    pub outcome: LookupOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LookupOutcome {
    /// A record: its relays (as published, before a server's relays-off
    /// setting drops them) and addresses.
    Found {
        relays: Vec<String>,
        addrs: Vec<String>,
    },
    /// The lookup answered, with no record of the server.
    NoRecord,
    /// The lookup could not be asked or failed.
    Error { message: String },
}

/// How a dial went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialReport {
    pub at: u64,
    /// How long it took, in milliseconds.
    pub took_ms: u64,
    pub connected: bool,
    /// Where it stopped; `None` when it connected.
    pub stage: Option<DialStage>,
    /// The error as the app was told it; `None` when it connected.
    pub error: Option<String>,
    /// The addresses and relays the dial was given.
    pub dialled: Vec<String>,
    /// Every path iroh knew of for the server when the dial ended, from
    /// what it was given and what lookups found. Not how each one fared:
    /// iroh does not say.
    pub known: Vec<KnownPath>,
}

/// The stage a dial stopped at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DialStage {
    /// Nothing to dial: no address or relay, and no lookup found a record.
    NoAddress,
    /// Iroh could not start the connection.
    Connect,
    /// No answer in time on any path.
    Timeout,
    /// The QUIC or TLS handshake failed.
    Handshake,
    /// The server closed or reset the connection as it was set up.
    Closed,
    /// It connected, and did not answer `Hello`.
    Hello,
    /// It answered `Hello` and refused: another protocol revision, not the
    /// server pinned, signed out, full.
    Refused,
}

/// A path to a server: an IP address or a relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownPath {
    pub kind: PathKind,
    /// `ip:port`, or the relay's URL.
    pub addr: String,
    /// In use by a connection then.
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    /// A private, link-local or loopback address.
    Lan,
    /// A public address.
    Direct,
    Relay,
}

impl PathKind {
    pub(crate) fn of(addr: &TransportAddr) -> Option<(Self, String)> {
        match addr {
            TransportAddr::Ip(ip) => Some((ip_kind(*ip), ip.to_string())),
            TransportAddr::Relay(url) => Some((Self::Relay, url.to_string())),
            _ => None,
        }
    }
}

fn ip_kind(addr: SocketAddr) -> PathKind {
    match ConnectionPath::of_ip(addr) {
        ConnectionPath::Lan { .. } => PathKind::Lan,
        _ => PathKind::Direct,
    }
}

/// A path of the connection open now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LivePath {
    pub kind: PathKind,
    pub addr: String,
    pub rtt_ms: u64,
    /// The one packets go by.
    pub selected: bool,
}

/// What lookups answered and how dials went, kept for this run.
#[derive(Debug, Default)]
pub(crate) struct ReachLog {
    lookups: Mutex<Vec<(EndpointId, LookupResult)>>,
    dials: Mutex<HashMap<EndpointId, DialReport>>,
}

impl ReachLog {
    /// `lookup` answered about `id`: it replaces that lookup's answer
    /// before, and the oldest answers go past [`MAX_LOOKUP_RESULTS`].
    pub(crate) fn looked_up(&self, id: EndpointId, lookup: &str, outcome: LookupOutcome) {
        let mut kept = self.lookups.lock();
        kept.retain(|(of, r)| !(*of == id && r.lookup == lookup));
        kept.push((
            id,
            LookupResult {
                lookup: lookup.to_owned(),
                at: servers::now(),
                outcome,
            },
        ));
        let over = kept.len().saturating_sub(MAX_LOOKUP_RESULTS);
        kept.drain(..over);
    }

    pub(crate) fn lookups_of(&self, id: &EndpointId) -> Vec<LookupResult> {
        self.lookups
            .lock()
            .iter()
            .filter(|(of, _)| of == id)
            .map(|(_, r)| r.clone())
            .collect()
    }

    /// `id`'s last dial was `report`; the oldest servers' go past
    /// [`MAX_DIALS`].
    pub(crate) fn dialled(&self, id: EndpointId, report: DialReport) {
        let mut dials = self.dials.lock();
        dials.insert(id, report);
        while dials.len() > MAX_DIALS {
            let Some(oldest) = dials.iter().min_by_key(|(_, r)| r.at).map(|(id, _)| *id) else {
                break;
            };
            dials.remove(&oldest);
        }
    }

    pub(crate) fn last_dial(&self, id: &EndpointId) -> Option<DialReport> {
        self.dials.lock().get(id).cloned()
    }
}

/// What a lookup's record says.
pub(crate) fn found(item: &Item) -> LookupOutcome {
    let info = item.endpoint_info();
    LookupOutcome::Found {
        relays: info
            .relay_urls()
            .take(MAX_LISTED)
            .map(|u| u.to_string())
            .collect(),
        addrs: info
            .ip_addrs()
            .take(MAX_LISTED)
            .map(|a| a.to_string())
            .collect(),
    }
}

pub(crate) fn saved_addrs(server: &servers::SavedServer) -> Vec<SavedAddr> {
    server
        .known_addrs
        .iter()
        .map(|k| SavedAddr {
            addr: k.addr.to_string(),
            reached: k.source == AddrSource::Reached,
            last_seen: k.last_seen,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;

    fn id(seed: u8) -> EndpointId {
        SecretKey::from_bytes(&[seed; 32]).public()
    }

    #[test]
    fn a_lookup_s_answer_replaces_its_last_and_the_oldest_go() {
        let log = ReachLog::default();
        log.looked_up(id(1), "mdns", LookupOutcome::NoRecord);
        log.looked_up(
            id(1),
            "https://dns.iroh.link/pkarr",
            LookupOutcome::Error {
                message: "dns failed".into(),
            },
        );
        log.looked_up(
            id(1),
            "mdns",
            LookupOutcome::Found {
                relays: vec![],
                addrs: vec!["192.168.1.2:7420".into()],
            },
        );
        let results = log.lookups_of(&id(1));
        assert_eq!(results.len(), 2, "{results:?}");
        assert!(matches!(
            results.iter().find(|r| r.lookup == "mdns").unwrap().outcome,
            LookupOutcome::Found { .. }
        ));
        assert!(log.lookups_of(&id(2)).is_empty());
        for n in 0..MAX_LOOKUP_RESULTS + 5 {
            log.looked_up(
                id(2),
                &format!("https://p{n}.example.net/"),
                LookupOutcome::NoRecord,
            );
        }
        // Bounded: server 1's, the oldest, are gone.
        assert!(log.lookups_of(&id(1)).is_empty());
        assert_eq!(log.lookups_of(&id(2)).len(), MAX_LOOKUP_RESULTS);
    }

    #[test]
    fn the_last_dial_of_at_most_so_many_servers_is_kept() {
        let log = ReachLog::default();
        let report = |at: u64| DialReport {
            at,
            took_ms: 1,
            connected: false,
            stage: Some(DialStage::Timeout),
            error: Some("no answer".into()),
            dialled: vec![],
            known: vec![],
        };
        for n in 0..=MAX_DIALS as u8 {
            log.dialled(id(n), report(100 + u64::from(n)));
        }
        // The oldest, server 0's, made room.
        assert_eq!(log.last_dial(&id(0)), None);
        assert_eq!(log.last_dial(&id(MAX_DIALS as u8)).unwrap().at, 132);
        log.dialled(id(5), report(500));
        assert_eq!(log.last_dial(&id(5)).unwrap().at, 500);
    }

    #[test]
    fn a_path_is_lan_direct_or_relay() {
        let lan = TransportAddr::Ip("192.168.86.10:7430".parse().unwrap());
        let direct = TransportAddr::Ip("[2607:fb90::1]:7430".parse().unwrap());
        let relay = TransportAddr::Relay("https://relay.example.net./".parse().unwrap());
        assert_eq!(PathKind::of(&lan).unwrap().0, PathKind::Lan);
        assert_eq!(PathKind::of(&direct).unwrap().0, PathKind::Direct);
        assert_eq!(
            PathKind::of(&relay).unwrap(),
            (PathKind::Relay, "https://relay.example.net./".into())
        );
    }

    #[test]
    fn a_report_is_plain_json() {
        let outcome = LookupOutcome::Error {
            message: "x".into(),
        };
        assert_eq!(
            serde_json::to_value(&outcome).unwrap(),
            serde_json::json!({"kind": "error", "message": "x"})
        );
        assert_eq!(
            serde_json::to_value(DialStage::NoAddress).unwrap(),
            "no_address"
        );
    }
}
