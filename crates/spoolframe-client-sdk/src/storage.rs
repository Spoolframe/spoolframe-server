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
    /// Secrets: the device's key and the session tokens.
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

    /// Secret: the sessions on one server, JSON.
    pub fn sessions(server: &str) -> String {
        format!("sessions/{server}")
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
