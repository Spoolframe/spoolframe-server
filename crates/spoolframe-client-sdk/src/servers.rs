//! Saved servers and their pins.
//!
//! A saved server is pinned to its `EndpointId`: that is its identity, and
//! the client only ever connects to that key. Its addresses are hints. A
//! server is saved only when the app pins it, after the user has seen its
//! fingerprint. A pin is replaced only by an explicit
//! `trust_new_identity`, never by a connect.
//!
//! The pin is the key with the server id the server answered with.
//! A connect needs both. A new name is saved; a new key with the
//! same id is an identity change, as above; the same key with a new id
//! is refused, as a server never changes its id.
//!
//! The addresses are every one the server was reached at, each
//! with when it was last seen, not only the last connection's: a phone
//! that reached the server over its public IPv6 last time must still know
//! the address it once reached it at over a VPN. A sighting adds to them;
//! one not seen for [`MAX_ADDR_AGE`] goes at the next sighting, and only
//! the newest [`MAX_KNOWN_ADDRS`] are kept.
//!
//! A signed-in device is also told the server's own addresses:
//! those on every interface it has, which this device may never have
//! reached (a VPN address while the phone is off the VPN). They are kept
//! the same way, marked [`AddrSource::Told`]. Each list the server tells
//! replaces the told addresses before it: one it no longer tells goes at
//! once. An address is told until the device reaches it; once reached it
//! stays reached while the list has it (see below). Under the cap a told
//! address never pushes out a reached one: every reached address is kept
//! first, newest first, and told ones fill what room is left, newest
//! first. So a server that tells many addresses (a host full of container
//! bridges) cannot make a device forget where it has really reached it.
//!
//! Once the server has told a list, a reached address it leaves out is
//! not kept, but for one: the path of the last connection
//! ([`SavedServer::last_path`]), which may be a port forward the server
//! was not told of. The rest go when the list comes, when a connection
//! succeeds over another path, and when the file is read. Iroh sends the
//! first packet to every address it is given and settles the handshake
//! on the first answer, so an address where another key answers now
//! fails the whole dial: what is saved is decided here, and a dial goes
//! to all of it at once. A server that never told a list keeps what it
//! was reached at, each for [`MAX_ADDR_AGE`]: nothing says which it
//! still has.

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use spoolframe_protocol::invite::{MAX_HINTS, is_relay_url};

use crate::storage::{Storage, StorageError, keys};

/// How long an address is kept without being seen again, in seconds.
pub(crate) const MAX_ADDR_AGE: u64 = 30 * 24 * 60 * 60;
/// How many addresses a server keeps: the newest.
pub(crate) const MAX_KNOWN_ADDRS: usize = 32;

/// How this device knows an address of a server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AddrSource {
    /// A connection reached the server there.
    #[default]
    Reached,
    /// The server said it has the address (the `ServerAddresses` event),
    /// and no connection has reached it there yet.
    Told,
}

/// An address a server was reached at or told, and when it last was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KnownAddr {
    pub addr: SocketAddr,
    /// Unix seconds.
    pub last_seen: u64,
    /// Absent from the file: reached.
    #[serde(default)]
    pub source: AddrSource,
}

/// A server as it is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedServer {
    pub name: String,
    pub endpoint_id: String,
    /// The server id pinned with the key: the pin is the pair. Taken
    /// at the first connect; `None` when the file has none, until the next
    /// connect pins it.
    #[serde(default)]
    pub server_id: Option<String>,
    /// Where it was reached, newest first. Changed only by
    /// [`SavedServer::saw`].
    #[serde(default)]
    pub known_addrs: Vec<KnownAddr>,
    /// The last connection's addresses, in a file that has only these
    /// (`last_known_addrs`). Read into `known_addrs` on load, and never written.
    #[serde(default, rename = "last_known_addrs", skip_serializing)]
    legacy_addrs: Vec<SocketAddr>,
    /// The user confirmed its fingerprint. Absent from the file: false.
    #[serde(default)]
    pub verified: bool,
    // Unix seconds. Each is absent from a file written before the SDK kept
    // it, and read as unknown: never guessed from the other fields.
    /// When this key was pinned.
    #[serde(default)]
    pub pinned_at: Option<u64>,
    /// When the user last confirmed its fingerprint; `None` while it is
    /// not verified.
    #[serde(default)]
    pub verified_at: Option<u64>,
    /// When it last answered a connect as itself.
    #[serde(default)]
    pub last_connected_at: Option<u64>,
    /// The URLs of the relays the server said it uses (revision 7), as its
    /// last `ServerAddresses` event listed them, and an invite's: what
    /// this device dials it through with relays on. Absent from the file:
    /// none.
    #[serde(default)]
    pub relays: Vec<String>,
    /// The pkarr relay the server publishes where it is to, as it
    /// or an invite told: where this device looks it up by its key. Absent
    /// from the file: none.
    #[serde(default)]
    pub lookup: Option<String>,
    /// The addresses of the server's last `ServerAddresses` event, in its
    /// order: what it advertises, for an app to show. Unlike
    /// `known_addrs`, never capped, aged or mixed with addresses reached.
    /// Absent from the file: none.
    #[serde(default)]
    pub advertised_addrs: Vec<SocketAddr>,
    /// When that event came, in Unix seconds; `None` if none has.
    #[serde(default)]
    pub advertised_at: Option<u64>,
    /// Whether this device reaches it through relays: the user's choice,
    /// or an invite's (redeeming or saving one whose link names relays
    /// chooses them); `None` follows [`crate::ClientOptions::relays`].
    /// Absent from the file: `None`.
    #[serde(default)]
    pub use_relays: Option<bool>,
    /// `relays` is the user's list ([`crate::Client::set_server_relay_urls`]):
    /// what the server advertises and what invites name do not change
    /// it. Absent from the file: false.
    #[serde(default)]
    pub relays_edited: bool,
    /// Where the last connect reached it: the server's address on the
    /// selected path, when that was an IP path; `None` after one over a
    /// relay. The one reached address kept though the server's list
    /// leaves it out (a port forward it was not told of, `network.wan`
    /// unset). Absent from the file: none, so the file keeps no address
    /// its server's list leaves out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_path: Option<SocketAddr>,
}

/// n0's public pkarr relay, where a server with `network.pkarr = "n0"`
/// publishes its record.
pub(crate) const N0_LOOKUP: &str = "https://dns.iroh.link/pkarr";

impl SavedServer {
    /// The relays and lookup the server told, held to what
    /// an invite link may carry: each a plain http(s) URL,
    /// once, at most [`MAX_HINTS`] relays. The rest is left out: a pinned
    /// server is trusted for its key, not to size what this device keeps.
    /// A list the user edited is kept as they left it; the lookup is
    /// taken either way.
    pub fn told_relays(&mut self, relays: Vec<String>, lookup: Option<String>) {
        if !self.relays_edited {
            self.relays = relay_list(relays);
        }
        self.lookup = lookup.filter(|url| is_relay_url(url));
    }

    /// Whether n0's lookup ([`N0_LOOKUP`]) is this device's only way to it
    /// off the LAN: reached through relays (its own choice, else
    /// `relays_on`, the client's), yet with no relay and no lookup known.
    /// So is a server saved while it had both off, until it next says
    /// otherwise, which it can only do over a connection.
    pub fn needs_n0_lookup(&self, relays_on: bool) -> bool {
        self.use_relays.unwrap_or(relays_on) && self.relays.is_empty() && self.lookup.is_none()
    }

    /// The user's own relay list from now on; empty gives it back to the
    /// server, which fills it at its next `ServerAddresses` event (and an
    /// invite's link meanwhile). Held as [`SavedServer::told_relays`]
    /// holds a told one; `false` if a URL is not a relay's.
    pub fn edit_relays(&mut self, relays: Vec<String>) -> bool {
        if !relays.iter().all(|r| is_relay_url(r)) {
            return false;
        }
        self.relays_edited = !relays.is_empty();
        self.relays = relay_list(relays);
        true
    }

    /// A key pinned now, from a connection to it just made at `addrs`.
    pub fn pinned_now(
        name: String,
        endpoint_id: String,
        server_id: String,
        addrs: Vec<SocketAddr>,
        verified: bool,
    ) -> Self {
        let now = now();
        let mut server = Self {
            name,
            endpoint_id,
            server_id: Some(server_id),
            known_addrs: Vec::new(),
            legacy_addrs: Vec::new(),
            verified,
            pinned_at: Some(now),
            verified_at: verified.then_some(now),
            last_connected_at: Some(now),
            relays: Vec::new(),
            lookup: None,
            advertised_addrs: Vec::new(),
            advertised_at: None,
            use_relays: None,
            relays_edited: false,
            last_path: None,
        };
        server.saw(addrs, now);
        server
    }

    /// The user confirmed its fingerprint now, or took it back.
    pub fn set_verified(&mut self, verified: bool) {
        self.verified = verified;
        self.verified_at = verified.then(now);
    }

    /// The server was reached at `addrs` at `now` (Unix seconds). They
    /// join the addresses known, dated `now`; then any not seen for
    /// [`MAX_ADDR_AGE`] go, and only the newest [`MAX_KNOWN_ADDRS`] stay.
    /// Whether that changed which addresses are known, not only their
    /// dates.
    pub fn saw(&mut self, addrs: impl IntoIterator<Item = SocketAddr>, now: u64) -> bool {
        self.sighted(addrs, now, AddrSource::Reached)
    }

    /// The server told this device at `now` that `addrs` is where it is:
    /// its whole list, which replaces the addresses it told before. An
    /// address only told before and not now goes; one reached stays,
    /// whether it is in the list or not. The new ones are kept as
    /// [`SavedServer::saw`] keeps addresses reached, but a reached address
    /// stays reached, and under the cap reached addresses go last.
    ///
    /// The list is also kept as it came, as [`Self::advertised_addrs`],
    /// dated `now`.
    pub fn told(&mut self, addrs: impl IntoIterator<Item = SocketAddr>, now: u64) -> bool {
        let addrs: Vec<SocketAddr> = addrs.into_iter().map(canonical).collect();
        self.advertised_addrs.clone_from(&addrs);
        self.advertised_at = Some(now);
        let before = self.known_addrs.len();
        self.known_addrs
            .retain(|k| k.source == AddrSource::Reached || addrs.contains(&k.addr));
        let dropped = self.known_addrs.len() != before;
        self.sighted(addrs, now, AddrSource::Told) || dropped
    }

    /// An invite named `addrs` as where the server is: they join
    /// the addresses known as told ones, as [`SavedServer::told`] keeps
    /// them, but replace nothing: an invite is not the server's whole
    /// list. The server's next list replaces them as it replaces any told
    /// address.
    pub fn hinted(&mut self, addrs: impl IntoIterator<Item = SocketAddr>, now: u64) -> bool {
        self.sighted(addrs, now, AddrSource::Told)
    }

    fn sighted(
        &mut self,
        addrs: impl IntoIterator<Item = SocketAddr>,
        now: u64,
        source: AddrSource,
    ) -> bool {
        let before = self.addrs();
        let mut known: Vec<KnownAddr> = Vec::new();
        for addr in addrs.into_iter().map(canonical) {
            if known.iter().any(|k| k.addr == addr) {
                continue;
            }
            let was_reached = self
                .known_addrs
                .iter()
                .any(|k| k.addr == addr && k.source == AddrSource::Reached);
            known.push(KnownAddr {
                addr,
                last_seen: now,
                source: if was_reached {
                    AddrSource::Reached
                } else {
                    source
                },
            });
        }
        let older: Vec<KnownAddr> = std::mem::take(&mut self.known_addrs)
            .into_iter()
            .filter(|old| !known.iter().any(|k| k.addr == old.addr))
            .collect();
        known.extend(older);
        // Stable: those seen together keep the order they were seen in.
        known.sort_by_key(|k| std::cmp::Reverse(k.last_seen));
        known.retain(|k| {
            let age = now.saturating_sub(k.last_seen);
            age <= MAX_ADDR_AGE && !self.left_out(k)
        });
        keep_reached_first(&mut known);
        self.known_addrs = known;
        let after = self.addrs();
        after.len() != before.len() || after.iter().any(|a| !before.contains(a))
    }

    /// The addresses known, newest first.
    pub fn addrs(&self) -> Vec<SocketAddr> {
        self.known_addrs.iter().map(|k| k.addr).collect()
    }

    /// Whether `known` is an address a connect reached that the server's
    /// list, once it has one, leaves out, and not the last path: one that
    /// is not kept.
    fn left_out(&self, known: &KnownAddr) -> bool {
        self.advertised_at.is_some()
            && known.source == AddrSource::Reached
            && !self.advertised_addrs.contains(&known.addr)
            && self.last_path != Some(known.addr)
    }

    /// A connect reached it just now over `path` ([`Self::last_path`]).
    /// The reached address it replaces goes at the next sighting, if the
    /// server's list leaves it out.
    pub fn connected_over(&mut self, path: Option<SocketAddr>) {
        self.last_path = path.map(canonical);
    }

    /// A dial failed at the handshake with the last path in it: something
    /// answered there, and not as this server, or the dial would have
    /// connected. It is no longer the last path, so if the server's list
    /// leaves it out it goes now. Whether an address went.
    pub fn lost_path(&mut self) -> bool {
        if self.last_path.take().is_none() {
            return false;
        }
        self.drop_left_out()
    }

    /// Drop the reached addresses [`Self::left_out`]. Whether any went.
    fn drop_left_out(&mut self) -> bool {
        let before = self.known_addrs.len();
        let kept: Vec<KnownAddr> = self
            .known_addrs
            .iter()
            .filter(|k| !self.left_out(k))
            .copied()
            .collect();
        self.known_addrs = kept;
        self.known_addrs.len() != before
    }

    /// A server from a file with only `last_known_addrs`: the last connection's
    /// addresses become known from `now`. Not from when it last
    /// connected, which may be over [`MAX_ADDR_AGE`] ago: they would be
    /// dropped at the first sighting of any other address.
    ///
    /// Reached addresses its server's list leaves out are dropped here,
    /// but the last path.
    fn migrate(&mut self, now: u64) {
        let legacy = std::mem::take(&mut self.legacy_addrs);
        if !legacy.is_empty() {
            self.saw(legacy, now);
        }
        self.drop_left_out();
    }
}

/// Cut `known`, newest first, to [`MAX_KNOWN_ADDRS`]: the newest reached
/// addresses, as many as fit, then the newest told ones in the room left.
/// The order is kept.
fn keep_reached_first(known: &mut Vec<KnownAddr>) {
    let reached = known
        .iter()
        .filter(|k| k.source == AddrSource::Reached)
        .count()
        .min(MAX_KNOWN_ADDRS);
    let mut room = [reached, MAX_KNOWN_ADDRS - reached];
    known.retain(|k| {
        let left = &mut room[usize::from(k.source == AddrSource::Told)];
        let keep = *left > 0;
        *left = left.saturating_sub(1);
        keep
    });
}

/// `addr`, an IPv4 address written as IPv6 (`::ffff:a.b.c.d`) as IPv4.
fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

/// Now, in Unix seconds.
pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `relays` as a server's relay list: each once, at most [`MAX_HINTS`],
/// only plain http(s) URLs.
fn relay_list(relays: Vec<String>) -> Vec<String> {
    let urls: Vec<String> = relays.into_iter().filter(|r| is_relay_url(r)).collect();
    crate::client::newest_first(&urls, &[], MAX_HINTS)
}

/// Unix seconds as a time.
pub(crate) fn time(secs: Option<u64>) -> Option<SystemTime> {
    secs.map(|s| UNIX_EPOCH + Duration::from_secs(s))
}

/// A server this device knows: saved, heard on the LAN, or both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerEntry {
    /// The server's identity, hex.
    pub endpoint_id: String,
    pub name: String,
    /// Where it was heard, reached, or said it is once signed in:
    /// every hint for dialing, never its identity.
    pub addrs: Vec<SocketAddr>,
    /// What the server advertises: the addresses of its latest
    /// `ServerAddresses` event, in the server's order, pushed while this
    /// device was signed in (revision 7). Empty if it never told this
    /// device, and for a server not saved. An address this device only
    /// reached is not in it.
    pub advertised_addrs: Vec<SocketAddr>,
    /// The relays this device reaches it through when its relays are on
    /// (`use_relays`): those its latest `ServerAddresses` event listed and
    /// an invite's link named, or the user's own list once edited
    /// (`relays_edited`). Empty while the server has relays off and none
    /// was named.
    pub relays: Vec<String>,
    /// `relays` is the user's ([`crate::Client::set_server_relay_urls`]):
    /// what the server advertises does not change it.
    pub relays_edited: bool,
    /// When that event came, to the second, for "as of" text. `None` if
    /// none has.
    pub advertised_at: Option<SystemTime>,
    /// The key's fingerprint: what the user compares with the
    /// server's `spoolframe-server fingerprint`.
    pub fingerprint: String,
    /// The server id: pinned with the key, or the one it answered
    /// with. `None` for a server only heard on the LAN, or saved with no server
    /// id and not connected to since. Its app passkeys are for
    /// `<server id>.spoolframe.app`.
    pub server_id: Option<String>,
    /// Saved, with this key pinned.
    pub pinned: bool,
    /// Pinned after the user confirmed the fingerprint; a pinned server
    /// that is not is shown as "Not verified".
    pub verified: bool,
    /// Heard on this network just now.
    pub on_network: bool,
    /// When this key was pinned. `None` for a server not saved, or when the
    /// date is not known.
    pub pinned_at: Option<SystemTime>,
    /// When the user last confirmed its fingerprint: at pinning, or a later
    /// "Verify now". `None` while not verified, or when the date
    /// is not known.
    pub verified_at: Option<SystemTime>,
    /// When it last answered a connect as itself. `None` for a server not
    /// saved, or when the date is not known.
    pub last_connected_at: Option<SystemTime>,
    /// Whether this device reaches it through relays: `Some` its own
    /// choice ([`crate::Client::set_server_relays`], or an invite's), `None`
    /// [`crate::ClientOptions::relays`]. `None` for a server not saved.
    pub use_relays: Option<bool>,
}

pub(crate) async fn load(storage: &dyn Storage) -> Result<Vec<SavedServer>, StorageError> {
    match storage.data(keys::SERVERS).await? {
        None => Ok(Vec::new()),
        Some(bytes) => {
            let mut servers: Vec<SavedServer> = serde_json::from_slice(&bytes)
                .map_err(|e| StorageError(format!("the saved servers cannot be read: {e}")))?;
            let now = now();
            for server in &mut servers {
                server.migrate(now);
            }
            Ok(servers)
        }
    }
}

pub(crate) async fn save(
    storage: &dyn Storage,
    servers: &[SavedServer],
) -> Result<(), StorageError> {
    let bytes = serde_json::to_vec(servers).expect("saved servers always serialize");
    storage.set_data(keys::SERVERS, Some(bytes)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryStorage;

    const ID: &str = "ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12";
    const SERVER_ID: &str = "mzxw6ytbojtg633c";

    /// A servers file as SDKs before these dates wrote it, one entry from
    /// before P1-21 too.
    const OLD_FILE: &str = r#"[
        {"name": "Nimble", "endpoint_id": "ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12",
         "last_known_addrs": ["192.168.1.10:4433"], "verified": true},
        {"name": "Cabin", "endpoint_id": "cd34"}
    ]"#;

    #[tokio::test]
    async fn an_old_file_keeps_every_server_with_its_dates_unknown() {
        let storage = MemoryStorage::default();
        storage
            .set_data(keys::SERVERS, Some(OLD_FILE.as_bytes().to_vec()))
            .await
            .unwrap();
        let servers = load(&storage).await.unwrap();
        // When the address counts as seen is the next test's.
        let seen = servers[0].known_addrs.first().map_or(0, |k| k.last_seen);
        assert_eq!(
            servers,
            [
                SavedServer {
                    name: "Nimble".into(),
                    endpoint_id: ID.into(),
                    server_id: None,
                    known_addrs: vec![KnownAddr {
                        addr: "192.168.1.10:4433".parse().unwrap(),
                        last_seen: seen,
                        source: AddrSource::Reached,
                    }],
                    legacy_addrs: Vec::new(),
                    verified: true,
                    pinned_at: None,
                    verified_at: None,
                    last_connected_at: None,
                    relays: Vec::new(),
                    lookup: None,
                    advertised_addrs: Vec::new(),
                    advertised_at: None,
                    use_relays: None,
                    relays_edited: false,
                    last_path: None,
                },
                SavedServer {
                    name: "Cabin".into(),
                    endpoint_id: "cd34".into(),
                    server_id: None,
                    known_addrs: Vec::new(),
                    legacy_addrs: Vec::new(),
                    verified: false,
                    pinned_at: None,
                    verified_at: None,
                    last_connected_at: None,
                    relays: Vec::new(),
                    lookup: None,
                    advertised_addrs: Vec::new(),
                    advertised_at: None,
                    use_relays: None,
                    relays_edited: false,
                    last_path: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn the_server_id_is_kept_with_the_key() {
        let storage = MemoryStorage::default();
        let server = server();
        assert_eq!(server.server_id.as_deref(), Some(SERVER_ID));
        save(&storage, std::slice::from_ref(&server)).await.unwrap();
        assert_eq!(load(&storage).await.unwrap(), [server]);
    }

    #[tokio::test]
    async fn the_dates_are_kept_with_the_servers() {
        let storage = MemoryStorage::default();
        let mut server = SavedServer::pinned_now(
            "Nimble".into(),
            ID.into(),
            SERVER_ID.into(),
            Vec::new(),
            false,
        );
        server.last_connected_at = Some(1_790_000_500);
        save(&storage, &[server.clone()]).await.unwrap();
        assert_eq!(load(&storage).await.unwrap(), [server]);
    }

    const DAY: u64 = 24 * 60 * 60;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn server() -> SavedServer {
        SavedServer::pinned_now(
            "Nimble".into(),
            ID.into(),
            SERVER_ID.into(),
            Vec::new(),
            false,
        )
    }

    /// What the server tells refreshes its relays until the user edits
    /// them; an empty edit gives them back.
    #[test]
    fn an_edited_relay_list_is_kept_from_what_the_server_tells() {
        let mut server = server();
        server.told_relays(vec!["https://a.example.net/".into()], None);
        assert_eq!(server.relays, ["https://a.example.net/"]);
        assert!(server.edit_relays(vec!["https://mine.example.net/".into()]));
        assert!(server.relays_edited);
        server.told_relays(vec!["https://b.example.net/".into()], None);
        assert_eq!(server.relays, ["https://mine.example.net/"]);
        assert!(!server.edit_relays(vec!["ftp://nope".into()]));
        assert_eq!(server.relays, ["https://mine.example.net/"]);
        assert!(server.edit_relays(Vec::new()));
        assert!(!server.relays_edited);
        server.told_relays(vec!["https://b.example.net/".into()], None);
        assert_eq!(server.relays, ["https://b.example.net/"]);
    }

    #[test]
    fn a_sighting_adds_to_the_addresses_and_never_replaces_them() {
        let mut server = server();
        server.saw(
            [addr("192.168.1.10:4433"), addr("[2001:db8::1]:4433")],
            1_000,
        );
        server.saw([addr("10.10.0.6:4433")], 2_000);
        assert_eq!(
            server.addrs(),
            [
                addr("10.10.0.6:4433"),
                addr("192.168.1.10:4433"),
                addr("[2001:db8::1]:4433"),
            ]
        );
    }

    #[test]
    fn an_address_seen_again_is_dated_again_and_listed_once() {
        let mut server = server();
        server.saw([addr("192.168.1.10:4433"), addr("10.10.0.6:4433")], 1_000);
        server.saw([addr("192.168.1.10:4433")], 2_000);
        assert_eq!(
            server.known_addrs,
            [
                KnownAddr {
                    addr: addr("192.168.1.10:4433"),
                    last_seen: 2_000,
                    source: AddrSource::Reached,
                },
                KnownAddr {
                    addr: addr("10.10.0.6:4433"),
                    last_seen: 1_000,
                    source: AddrSource::Reached,
                },
            ]
        );
    }

    #[test]
    fn an_ipv4_address_in_ipv6_form_is_the_same_address() {
        let mut server = server();
        server.saw([addr("192.168.1.10:4433")], 1_000);
        server.saw([addr("[::ffff:192.168.1.10]:4433")], 2_000);
        assert_eq!(server.addrs(), [addr("192.168.1.10:4433")]);
    }

    #[test]
    fn a_sighting_says_whether_an_address_was_new() {
        let mut server = server();
        assert!(server.saw([addr("192.168.1.10:4433")], 1_000));
        assert!(!server.saw([addr("192.168.1.10:4433")], 2_000));
        assert!(server.saw([addr("10.10.0.6:4433")], 3_000));
    }

    #[test]
    fn an_address_not_seen_for_30_days_is_dropped_at_the_next_sighting() {
        let mut server = server();
        server.saw([addr("10.10.0.6:4433")], 1_000);
        server.saw([addr("192.168.1.10:4433")], 1_000 + 30 * DAY);
        assert_eq!(
            server.addrs(),
            [addr("192.168.1.10:4433"), addr("10.10.0.6:4433")],
            "30 days is still kept"
        );
        server.saw([addr("192.168.1.10:4433")], 1_000 + 30 * DAY + 1);
        assert_eq!(server.addrs(), [addr("192.168.1.10:4433")]);
    }

    #[test]
    fn the_newest_32_addresses_are_kept() {
        let mut server = server();
        for port in 0..40u16 {
            server.saw(
                [SocketAddr::from(([10, 0, 0, 1], 1000 + port))],
                1_000 + u64::from(port),
            );
        }
        let kept = server.addrs();
        assert_eq!(kept.len(), MAX_KNOWN_ADDRS);
        assert_eq!(kept.first(), Some(&SocketAddr::from(([10, 0, 0, 1], 1039))));
        assert_eq!(kept.last(), Some(&SocketAddr::from(([10, 0, 0, 1], 1008))));
    }

    #[test]
    fn a_clock_set_back_does_not_drop_what_was_seen_later() {
        let mut server = server();
        server.saw([addr("10.10.0.6:4433")], 1_000 + 40 * DAY);
        server.saw([addr("192.168.1.10:4433")], 1_000);
        assert_eq!(
            server.addrs(),
            [addr("10.10.0.6:4433"), addr("192.168.1.10:4433")]
        );
    }

    #[tokio::test]
    async fn the_addresses_and_their_dates_are_kept_with_the_servers() {
        let storage = MemoryStorage::default();
        let mut server = server();
        server.saw(
            [addr("192.168.1.10:4433"), addr("10.10.0.6:4433")],
            1_790_000_000,
        );
        save(&storage, &[server.clone()]).await.unwrap();
        assert_eq!(load(&storage).await.unwrap(), [server]);
    }

    #[tokio::test]
    async fn the_last_connections_addresses_from_an_old_file_are_known_from_now() {
        let storage = MemoryStorage::default();
        storage
            .set_data(keys::SERVERS, Some(OLD_FILE.as_bytes().to_vec()))
            .await
            .unwrap();
        let before = now();
        let servers = load(&storage).await.unwrap();
        let after = now();
        let [seen] = servers[0].known_addrs[..] else {
            panic!("{:?}", servers[0].known_addrs);
        };
        assert_eq!(seen.addr, addr("192.168.1.10:4433"));
        assert!((before..=after).contains(&seen.last_seen));

        // Saved again, the file has them in the new form only.
        save(&storage, &servers).await.unwrap();
        let file = String::from_utf8(storage.data(keys::SERVERS).await.unwrap().unwrap()).unwrap();
        assert!(!file.contains("last_known_addrs"), "{file}");
        assert_eq!(load(&storage).await.unwrap(), servers);
    }

    fn sources(server: &SavedServer) -> Vec<(SocketAddr, AddrSource)> {
        server
            .known_addrs
            .iter()
            .map(|k| (k.addr, k.source))
            .collect()
    }

    #[test]
    fn a_told_address_is_kept_as_told_until_it_is_reached() {
        let mut server = server();
        server.saw([addr("192.168.1.10:4433")], 1_000);
        assert!(server.told([addr("192.168.1.10:4433"), addr("10.10.0.2:4433")], 2_000));
        assert_eq!(
            sources(&server),
            [
                (addr("192.168.1.10:4433"), AddrSource::Reached),
                (addr("10.10.0.2:4433"), AddrSource::Told),
            ],
            "told again, a reached address stays reached"
        );
        server.saw([addr("10.10.0.2:4433")], 3_000);
        assert_eq!(
            sources(&server),
            [
                (addr("10.10.0.2:4433"), AddrSource::Reached),
                (addr("192.168.1.10:4433"), AddrSource::Reached),
            ]
        );
    }

    #[test]
    fn a_told_address_not_told_for_30_days_is_dropped() {
        let mut server = server();
        server.told([addr("10.10.0.2:4433")], 1_000);
        server.connected_over(Some(addr("192.168.1.10:4433")));
        server.saw([addr("192.168.1.10:4433")], 1_000 + 30 * DAY + 1);
        assert_eq!(server.addrs(), [addr("192.168.1.10:4433")]);
    }

    #[test]
    fn told_addresses_never_push_out_reached_ones() {
        let mut server = server();
        let reached: Vec<SocketAddr> = (0..20u16)
            .map(|p| SocketAddr::from(([192, 168, 1, 10], 1000 + p)))
            .collect();
        server.saw(reached.clone(), 1_000);
        // Newer, and more than the cap on their own; the list has the
        // reached ones too, last.
        let told: Vec<SocketAddr> = (0..40u16)
            .map(|p| SocketAddr::from(([172, 17, 0, 1], 2000 + p)))
            .collect();
        server.told(told.iter().chain(&reached).copied(), 2_000);
        let kept = server.addrs();
        assert_eq!(kept.len(), MAX_KNOWN_ADDRS);
        assert!(reached.iter().all(|a| kept.contains(a)), "{kept:?}");
        assert_eq!(
            kept.iter().filter(|a| told.contains(a)).count(),
            MAX_KNOWN_ADDRS - reached.len()
        );
        // In the list's order all the same.
        assert_eq!(kept.first(), Some(&told[0]));
    }

    #[test]
    fn reached_addresses_push_out_told_ones_and_each_other_by_age() {
        let mut server = server();
        // An invite's hint: the server has told no list, so every address
        // reached is kept.
        server.hinted([addr("10.10.0.2:4433")], 1_000);
        for port in 0..40u16 {
            server.saw(
                [SocketAddr::from(([192, 168, 1, 10], 1000 + port))],
                2_000 + u64::from(port),
            );
        }
        let kept = server.addrs();
        assert_eq!(kept.len(), MAX_KNOWN_ADDRS);
        assert!(!kept.contains(&addr("10.10.0.2:4433")));
        assert_eq!(kept.first(), Some(&addr("192.168.1.10:1039")));
    }

    #[tokio::test]
    async fn an_address_saved_before_sources_were_kept_was_reached() {
        let storage = MemoryStorage::default();
        let file = format!(
            r#"[{{"name": "Nimble", "endpoint_id": "{ID}",
                 "known_addrs": [{{"addr": "192.168.1.10:4433", "last_seen": 1000}}]}}]"#
        );
        storage
            .set_data(keys::SERVERS, Some(file.into_bytes()))
            .await
            .unwrap();
        let servers = load(&storage).await.unwrap();
        assert_eq!(
            sources(&servers[0]),
            [(addr("192.168.1.10:4433"), AddrSource::Reached)]
        );
    }

    #[tokio::test]
    async fn told_addresses_are_kept_as_told() {
        let storage = MemoryStorage::default();
        let mut server = server();
        server.saw([addr("192.168.1.10:4433")], 1_790_000_000);
        server.told([addr("10.10.0.2:4433")], 1_790_000_000);
        save(&storage, &[server.clone()]).await.unwrap();
        assert_eq!(load(&storage).await.unwrap(), [server]);
    }

    /// Each push is the server's whole list (revision 7): a told address
    /// it no longer tells is gone at once, not 30 days later. Where this
    /// device last reached the server stays, told or not.
    #[test]
    fn a_new_told_list_replaces_the_told_addresses_and_keeps_the_last_path() {
        let mut server = server();
        server.connected_over(Some(addr("192.168.1.10:4433")));
        server.saw([addr("192.168.1.10:4433")], 1_000);
        server.told(
            [
                addr("192.168.1.10:4433"),
                addr("172.17.0.1:4433"),
                addr("10.10.0.2:4433"),
            ],
            2_000,
        );
        assert!(server.told([addr("10.10.0.2:4433")], 3_000));
        assert_eq!(
            sources(&server),
            [
                (addr("10.10.0.2:4433"), AddrSource::Told),
                (addr("192.168.1.10:4433"), AddrSource::Reached),
            ]
        );
    }

    /// An invite's hints (P6-01) join as told addresses and replace none:
    /// unlike the server's own list, an invite is not the whole of it.
    #[test]
    fn an_invite_s_hints_join_as_told_and_replace_nothing() {
        let mut server = server();
        server.connected_over(Some(addr("192.168.1.10:4433")));
        server.saw([addr("192.168.1.10:4433")], 1_000);
        server.told([addr("10.10.0.2:4433")], 2_000);
        assert!(server.hinted([addr("203.0.113.9:7420")], 3_000));
        assert_eq!(
            sources(&server),
            [
                (addr("203.0.113.9:7420"), AddrSource::Told),
                (addr("10.10.0.2:4433"), AddrSource::Told),
                (addr("192.168.1.10:4433"), AddrSource::Reached),
            ]
        );
        assert_eq!(server.advertised_addrs, [addr("10.10.0.2:4433")]);
    }

    #[test]
    fn an_empty_told_list_leaves_only_the_last_path() {
        let mut server = server();
        server.connected_over(Some(addr("192.168.1.10:4433")));
        server.saw([addr("192.168.1.10:4433")], 1_000);
        server.told([addr("172.17.0.1:4433")], 2_000);
        assert!(server.told([], 3_000));
        assert_eq!(server.addrs(), [addr("192.168.1.10:4433")]);
    }

    /// The Linux client's Fulcrum: reached at a dev server's port that a
    /// server of another key took since. The server's list leaves it out,
    /// so it goes when the list comes.
    #[test]
    fn a_told_list_drops_the_reached_addresses_it_leaves_out() {
        let mut server = server();
        server.saw([addr("10.10.0.6:7430"), addr("10.10.0.1:7420")], 1_000);
        assert!(server.told([addr("10.10.0.1:7420"), addr("192.168.86.10:7420")], 2_000));
        assert_eq!(
            sources(&server),
            [
                (addr("10.10.0.1:7420"), AddrSource::Reached),
                (addr("192.168.86.10:7420"), AddrSource::Told),
            ]
        );
    }

    /// A server that never told a list: nothing says which it still has,
    /// so what it was reached at is kept the 30 days.
    #[test]
    fn a_server_that_never_told_keeps_every_address_reached() {
        let mut server = server();
        server.saw([addr("10.10.0.6:7430")], 1_000);
        server.connected_over(Some(addr("10.10.0.1:7420")));
        server.saw([addr("10.10.0.1:7420")], 1_000 + 8 * DAY);
        assert_eq!(
            server.addrs(),
            [addr("10.10.0.1:7420"), addr("10.10.0.6:7430")]
        );
    }

    /// A port forward the server was not told of: reached there last,
    /// though its list leaves it out. It is kept, and is the only such
    /// address kept; a connection over another path lets it go.
    #[test]
    fn the_last_path_is_the_one_address_kept_that_the_list_leaves_out() {
        let mut server = server();
        server.saw([addr("203.0.113.7:7420"), addr("198.51.100.4:7420")], 1_000);
        server.connected_over(Some(addr("203.0.113.7:7420")));
        server.told([addr("10.10.0.1:7420")], 2_000);
        assert_eq!(
            server.addrs(),
            [addr("10.10.0.1:7420"), addr("203.0.113.7:7420")]
        );

        server.connected_over(Some(addr("10.10.0.1:7420")));
        server.saw([addr("10.10.0.1:7420")], 3_000);
        assert_eq!(server.addrs(), [addr("10.10.0.1:7420")]);
    }

    /// Over a relay there is no last path: no address the list leaves out
    /// is kept.
    #[test]
    fn a_connection_over_a_relay_keeps_no_address_the_list_leaves_out() {
        let mut server = server();
        server.saw([addr("203.0.113.7:7420")], 1_000);
        server.connected_over(Some(addr("203.0.113.7:7420")));
        server.told([addr("10.10.0.1:7420")], 2_000);
        server.connected_over(None);
        server.saw([], 3_000);
        assert_eq!(server.addrs(), [addr("10.10.0.1:7420")]);
    }

    /// The last path that failed a dial at the handshake is let go, and
    /// with it the address, if the server's list leaves it out; one the
    /// list has stays.
    #[test]
    fn a_lost_last_path_goes_unless_the_list_has_it() {
        let mut server = server();
        server.saw([addr("10.10.0.6:7430"), addr("10.10.0.1:7420")], 1_000);
        server.connected_over(Some(addr("10.10.0.6:7430")));
        server.told([addr("10.10.0.1:7420")], 2_000);
        assert!(server.lost_path());
        assert_eq!(server.last_path, None);
        assert_eq!(server.addrs(), [addr("10.10.0.1:7420")]);

        server.connected_over(Some(addr("10.10.0.1:7420")));
        assert!(!server.lost_path(), "the list has it");
        assert_eq!(server.addrs(), [addr("10.10.0.1:7420")]);
    }

    /// Fulcrum's file as the SDK left it before: eleven reached addresses
    /// on a port another key holds now, which the server's list leaves
    /// out. They are gone when it is read; with a last path among them,
    /// that one alone is kept.
    #[tokio::test]
    async fn a_poisoned_file_keeps_only_what_the_list_has_and_the_last_path() {
        let stale: Vec<String> = (1..=11).map(|n| format!("10.10.0.{n}:7430")).collect();
        let file = |last_path: Option<&str>| {
            let mut known: Vec<serde_json::Value> = stale
                .iter()
                .map(|a| serde_json::json!({"addr": a, "last_seen": 2_000, "source": "reached"}))
                .collect();
            known.push(serde_json::json!(
                {"addr": "10.10.0.1:7420", "last_seen": 1_000, "source": "told"}
            ));
            let mut server = serde_json::json!({
                "name": "Fulcrum", "endpoint_id": ID, "server_id": SERVER_ID,
                "known_addrs": known,
                "advertised_addrs": ["10.10.0.1:7420"], "advertised_at": 1_000,
            });
            if let Some(path) = last_path {
                server["last_path"] = serde_json::json!(path);
            }
            serde_json::to_vec(&[server]).unwrap()
        };
        let storage = MemoryStorage::default();
        storage
            .set_data(keys::SERVERS, Some(file(None)))
            .await
            .unwrap();
        let servers = load(&storage).await.unwrap();
        assert_eq!(servers[0].addrs(), [addr("10.10.0.1:7420")]);

        storage
            .set_data(keys::SERVERS, Some(file(Some("10.10.0.6:7430"))))
            .await
            .unwrap();
        let servers = load(&storage).await.unwrap();
        assert_eq!(
            servers[0].addrs(),
            [addr("10.10.0.6:7430"), addr("10.10.0.1:7420")]
        );
    }

    /// P6-07, glm E-3: what a server pushes is held to an invite's limits.
    #[test]
    fn told_relays_are_urls_once_and_at_most_the_hint_limit() {
        let mut server = server();
        let mut pushed: Vec<String> = (0..100)
            .map(|n| format!("https://relay{n}.example.net/"))
            .collect();
        pushed.insert(0, "javascript:alert(1)".into());
        pushed.insert(1, "https://relay0.example.net/".into());
        pushed.insert(2, "https://bad host/".into());
        server.told_relays(pushed, Some("file:///etc/passwd".into()));
        assert_eq!(server.relays.len(), MAX_HINTS);
        assert_eq!(server.relays[0], "https://relay0.example.net/");
        assert_eq!(server.relays[1], "https://relay1.example.net/");
        assert_eq!(server.lookup, None);
        server.told_relays(vec![], Some("https://dns.iroh.link/pkarr".into()));
        assert_eq!(
            server.lookup.as_deref(),
            Some("https://dns.iroh.link/pkarr")
        );
    }

    /// A server saved with relays and its record off, which turned both on
    /// since (the tester's on 5G): n0's lookup until it tells its own.
    #[test]
    fn a_server_with_no_relay_or_lookup_is_looked_up_at_n0_while_relays_are_on() {
        let mut server = server();
        assert!(server.needs_n0_lookup(true));
        assert!(!server.needs_n0_lookup(false));
        server.use_relays = Some(false);
        assert!(!server.needs_n0_lookup(true));
        server.use_relays = Some(true);
        assert!(server.needs_n0_lookup(false));
        server.told_relays(vec!["https://relay.example.net/".into()], None);
        assert!(!server.needs_n0_lookup(true));
        server.told_relays(vec![], Some("https://pkarr.example.net/".into()));
        assert!(!server.needs_n0_lookup(true));
    }

    #[tokio::test]
    async fn the_relays_told_are_kept_with_the_server() {
        let storage = MemoryStorage::default();
        let mut server = server();
        server.relays = vec!["https://relay.example.net./".into()];
        save(&storage, std::slice::from_ref(&server)).await.unwrap();
        assert_eq!(load(&storage).await.unwrap(), [server]);
    }

    #[tokio::test]
    async fn a_server_saved_before_relays_were_kept_has_none() {
        let storage = MemoryStorage::default();
        let file = format!(
            r#"[{{"name": "Nimble", "endpoint_id": "{ID}", "server_id": "{SERVER_ID}",
                 "known_addrs": [{{"addr": "192.168.1.10:4433", "last_seen": 1000, "source": "told"}}],
                 "verified": true, "pinned_at": 1, "verified_at": 1, "last_connected_at": 1}}]"#
        );
        storage
            .set_data(keys::SERVERS, Some(file.into_bytes()))
            .await
            .unwrap();
        let servers = load(&storage).await.unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].relays, Vec::<String>::new());
        assert_eq!(servers[0].advertised_addrs, Vec::<SocketAddr>::new());
        assert_eq!(servers[0].advertised_at, None);
        assert_eq!(servers[0].addrs(), [addr("192.168.1.10:4433")]);
    }

    #[test]
    fn pinning_dates_the_pin_the_connect_and_a_verification_only_if_there_was_one() {
        let before = now();
        let unverified = SavedServer::pinned_now(
            "Nimble".into(),
            ID.into(),
            SERVER_ID.into(),
            Vec::new(),
            false,
        );
        let after = now();
        let pinned = unverified.pinned_at.unwrap();
        assert!((before..=after).contains(&pinned));
        assert_eq!(unverified.last_connected_at, Some(pinned));
        assert_eq!(unverified.verified_at, None);

        let verified = SavedServer::pinned_now(
            "Nimble".into(),
            ID.into(),
            SERVER_ID.into(),
            Vec::new(),
            true,
        );
        assert_eq!(verified.verified_at, verified.pinned_at);
    }

    #[test]
    fn verifying_later_dates_it_and_taking_it_back_clears_it() {
        let mut server = SavedServer::pinned_now(
            "Nimble".into(),
            ID.into(),
            SERVER_ID.into(),
            Vec::new(),
            false,
        );
        server.pinned_at = Some(1);
        server.set_verified(true);
        assert!(server.verified_at.is_some_and(|at| at > 1));
        assert_eq!(server.pinned_at, Some(1));
        server.set_verified(false);
        assert_eq!((server.verified, server.verified_at), (false, None));
    }
}
