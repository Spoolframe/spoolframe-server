//! Saved servers and their pins (P1-13, §19).
//!
//! A saved server is pinned to its `EndpointId`: that is its identity, and
//! the client only ever connects to that key. Its addresses are hints that
//! are refreshed on every connect. A server is saved only when the app pins
//! it, after the user has seen its fingerprint (P1-21). A pin is replaced
//! only by an explicit `trust_new_identity`, never by a connect (P1-13).

use std::net::SocketAddr;

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
