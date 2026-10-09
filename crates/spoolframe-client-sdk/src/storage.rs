//! Where the SDK keeps what must outlive the app: implemented by the app,
//! because only it knows the platform. The Linux client keeps secrets in
//! Secret Service and data under the XDG data directory; tvOS uses the
//! keychain (P5).

use parking_lot::Mutex;
use std::collections::HashMap;

use async_trait::async_trait;

/// The storage could not be read or written. Never treated as "empty":
/// a key that cannot be read is not replaced by a new one (§17).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("storage: {0}")]
pub struct StorageError(pub String);

/// Two key-value stores. `Ok(None)` means the key is not there; anything
/// that stops the store from answering is an error.
#[async_trait]
pub trait Storage: Send + Sync {
    /// Secrets: the device's key, the session tokens, portable roots and
    /// portable device keys.
    async fn secret(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError>;
    /// Set a secret; `None` removes it.
    async fn set_secret(&self, key: &str, value: Option<Vec<u8>>) -> Result<(), StorageError>;

    /// Plain data: the saved servers and their pins.
    async fn data(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError>;
    /// Set plain data; `None` removes it.
    async fn set_data(&self, key: &str, value: Option<Vec<u8>>) -> Result<(), StorageError>;
}

/// The keys the SDK uses. The values are the SDK's business; an app
/// stores them as given.
pub mod keys {
    /// Secret: the device's Iroh secret key, 32 bytes.
    pub const ENDPOINT_KEY: &str = "endpoint-key";
    /// Data: the saved servers, JSON.
    pub const SERVERS: &str = "servers";

    /// Data: the portable identities on this device, JSON (P7-01).
    pub const PORTABLE_IDENTITIES: &str = "portable-identities";

    /// Data: the device credentials this device acts with, JSON: each
    /// identity's id → its credential (P7-02). The identity's root need
    /// not be on this device.
    pub const PORTABLE_CREDENTIALS: &str = "portable-credentials";

    /// Secret: the sessions on one server, JSON.
    pub fn sessions(server: &str) -> String {
        format!("sessions/{server}")
    }

    /// Secret: one portable identity's root signing key, 32 bytes
    /// (P7-01). The most valuable secret the SDK keeps: whoever reads it
    /// is that user everywhere, until they recover elsewhere and revoke.
    pub fn portable_root(id: &str) -> String {
        format!("portable-root/{id}")
    }

    /// Secret: this device's own key for acting as portable identity
    /// `id`, 32 bytes (P7-02). Its credential names the public half.
    pub fn portable_device(id: &str) -> String {
        format!("portable-device/{id}")
    }

    /// Data: the last serial the root of `id` gave a credential, JSON
    /// (P7-02). Saved before each credential is signed, so no two share
    /// one.
    pub fn portable_serial(id: &str) -> String {
        format!("portable-serial/{id}")
    }

    /// Data: the newest identity state of `id` this device has, JSON
    /// (P7-02): made here by its root, or brought from elsewhere.
    pub fn portable_state(id: &str) -> String {
        format!("portable-state/{id}")
    }

    /// Data: every device key of `id` revoked here, and when, JSON (key →
    /// Unix seconds): the union of all the states this device has made or
    /// accepted, older revisions too (P7-02).
    pub fn portable_revoked(id: &str) -> String {
        format!("portable-revoked/{id}")
    }
}

/// Storage in memory: lost when dropped. For tests, and as the simplest
/// example of the trait.
#[derive(Debug, Default)]
pub struct MemoryStorage {
    secrets: Mutex<HashMap<String, Vec<u8>>>,
    data: Mutex<HashMap<String, Vec<u8>>>,
}

fn get(map: &Mutex<HashMap<String, Vec<u8>>>, key: &str) -> Option<Vec<u8>> {
    map.lock().get(key).cloned()
}

fn set(map: &Mutex<HashMap<String, Vec<u8>>>, key: &str, value: Option<Vec<u8>>) {
    let mut map = map.lock();
    match value {
        Some(value) => map.insert(key.to_owned(), value),
        None => map.remove(key),
    };
}

#[async_trait]
impl Storage for MemoryStorage {
    async fn secret(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(get(&self.secrets, key))
    }

    async fn set_secret(&self, key: &str, value: Option<Vec<u8>>) -> Result<(), StorageError> {
        set(&self.secrets, key, value);
        Ok(())
    }

    async fn data(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(get(&self.data, key))
    }

    async fn set_data(&self, key: &str, value: Option<Vec<u8>>) -> Result<(), StorageError> {
        set(&self.data, key, value);
        Ok(())
    }
}
