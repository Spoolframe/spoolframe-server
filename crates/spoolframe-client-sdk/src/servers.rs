//! Saved servers and their pins (P1-13, §19).
//!
//! A saved server is pinned to its `EndpointId`: that is its identity, and
//! the client only ever connects to that key. Its addresses are hints that
//! are refreshed on every connect. A server is saved only when the app pins
//! it, after the user has seen its fingerprint (P1-21). A pin is replaced
//! only by an explicit `trust_new_identity`, never by a connect (P1-13).

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::storage::{Storage, StorageError, keys};

/// A server as it is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavedServer {
    pub name: String,
    pub endpoint_id: String,
    #[serde(default)]
    pub last_known_addrs: Vec<SocketAddr>,
    /// The user confirmed its fingerprint. Pins from before P1-21 were
    /// never checked: false.
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
}

impl SavedServer {
    /// A key pinned now, from a connection to it just made.
    pub fn pinned_now(
        name: String,
        endpoint_id: String,
        last_known_addrs: Vec<SocketAddr>,
        verified: bool,
    ) -> Self {
        let now = now();
        Self {
            name,
            endpoint_id,
            last_known_addrs,
            verified,
            pinned_at: Some(now),
            verified_at: verified.then_some(now),
            last_connected_at: Some(now),
        }
    }

    /// The user confirmed its fingerprint now, or took it back.
    pub fn set_verified(&mut self, verified: bool) {
        self.verified = verified;
        self.verified_at = verified.then(now);
    }
}

/// Now, in Unix seconds.
pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
    /// Where it was heard or last connected to: hints for dialing, never
    /// its identity.
    pub addrs: Vec<SocketAddr>,
    /// The key's fingerprint (P1-21): what the user compares with the
    /// server's `spoolframe-server fingerprint`.
    pub fingerprint: String,
    /// Saved, with this key pinned.
    pub pinned: bool,
    /// Pinned after the user confirmed the fingerprint; a pinned server
    /// that is not is shown as "Not verified".
    pub verified: bool,
    /// Heard on this network just now.
    pub on_network: bool,
    /// When this key was pinned. `None` for a server not saved, or saved
    /// before the SDK kept the date.
    pub pinned_at: Option<SystemTime>,
    /// When the user last confirmed its fingerprint: at pinning, or a later
    /// "Verify now". `None` while not verified, or verified before the SDK
    /// kept the date.
    pub verified_at: Option<SystemTime>,
    /// When it last answered a connect as itself. `None` for a server not
    /// saved, or not connected to since the SDK kept the date.
    pub last_connected_at: Option<SystemTime>,
}

pub(crate) async fn load(storage: &dyn Storage) -> Result<Vec<SavedServer>, StorageError> {
    match storage.data(keys::SERVERS).await? {
        None => Ok(Vec::new()),
        Some(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| StorageError(format!("the saved servers cannot be read: {e}"))),
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
        assert_eq!(
            servers,
            [
                SavedServer {
                    name: "Nimble".into(),
                    endpoint_id: ID.into(),
                    last_known_addrs: vec!["192.168.1.10:4433".parse().unwrap()],
                    verified: true,
                    pinned_at: None,
                    verified_at: None,
                    last_connected_at: None,
                },
                SavedServer {
                    name: "Cabin".into(),
                    endpoint_id: "cd34".into(),
                    last_known_addrs: Vec::new(),
                    verified: false,
                    pinned_at: None,
                    verified_at: None,
                    last_connected_at: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn the_dates_are_kept_with_the_servers() {
        let storage = MemoryStorage::default();
        let mut server = SavedServer::pinned_now("Nimble".into(), ID.into(), Vec::new(), false);
        server.last_connected_at = Some(1_790_000_500);
        save(&storage, &[server.clone()]).await.unwrap();
        assert_eq!(load(&storage).await.unwrap(), [server]);
    }

    #[test]
    fn pinning_dates_the_pin_the_connect_and_a_verification_only_if_there_was_one() {
        let before = now();
        let unverified = SavedServer::pinned_now("Nimble".into(), ID.into(), Vec::new(), false);
        let after = now();
        let pinned = unverified.pinned_at.unwrap();
        assert!((before..=after).contains(&pinned));
        assert_eq!(unverified.last_connected_at, Some(pinned));
        assert_eq!(unverified.verified_at, None);

        let verified = SavedServer::pinned_now("Nimble".into(), ID.into(), Vec::new(), true);
        assert_eq!(verified.verified_at, verified.pinned_at);
    }

    #[test]
    fn verifying_later_dates_it_and_taking_it_back_clears_it() {
        let mut server = SavedServer::pinned_now("Nimble".into(), ID.into(), Vec::new(), false);
        server.pinned_at = Some(1);
        server.set_verified(true);
        assert!(server.verified_at.is_some_and(|at| at > 1));
        assert_eq!(server.pinned_at, Some(1));
        server.set_verified(false);
        assert_eq!((server.verified, server.verified_at), (false, None));
    }
}
