//! Where the SDK keeps what must outlive the app: implemented by the app,
//! because only it knows the platform. For example, keep secrets in the
//! Secret Service and data under the XDG data directory on Linux, and both
//! in the Keychain on Apple platforms.

pub mod files;

use parking_lot::Mutex;
use std::collections::HashMap;

use async_trait::async_trait;

/// The storage could not be read or written. Never treated as "empty":
/// a key that cannot be read is not replaced by a new one.
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

    /// Data: the portable identities on this device, JSON.
    pub const PORTABLE_IDENTITIES: &str = "portable-identities";

    /// Data: the device credentials this device acts with, JSON: each
    /// identity's id → its credential. The identity's root need
    /// not be on this device.
    pub const PORTABLE_CREDENTIALS: &str = "portable-credentials";

    /// Data: whether this device lets people sign in on it with an
    /// account that is not this device's own, JSON `true` or
    /// `false`; not there is `false`. The gate for every external
    /// principal, such as a portable identity paired by QR.
    pub const ALLOW_GUESTS: &str = "allow-guests";

    /// Data: the profile browsed last, JSON: its server and username.
    pub const LAST_PROFILE: &str = "last-profile";

    /// Secret: each profile's lock offline, JSON: whether its PIN
    /// is asked for on this device, and a hash of the PIN to check.
    pub const OFFLINE_LOCKS: &str = "offline-locks";

    /// Secret: the sessions on one server, JSON.
    pub fn sessions(server: &str) -> String {
        format!("sessions/{server}")
    }

    /// Secret: one portable identity's root signing key, 32 bytes.
    /// The most valuable secret the SDK keeps: whoever reads it
    /// is that user everywhere, until they recover elsewhere and revoke.
    /// This device only, never synced or backed up off the device: an
    /// identity whose root the user chose to sync keeps it under
    /// [`portable_root_synced`] instead.
    pub fn portable_root(id: &str) -> String {
        format!("portable-root/{id}")
    }

    /// Secret: this device's own key for acting as portable identity
    /// `id`, 32 bytes. Its credential names the public half.
    pub fn portable_device(id: &str) -> String {
        format!("portable-device/{id}")
    }

    /// Data: the last serial the root of `id` gave a credential here, JSON.
    /// Saved before each credential is signed. The next is the
    /// issue time in milliseconds, or one past this if that is larger, so
    /// no two from this device share one, and two devices that hold one
    /// root almost never do. Never synced: each holder keeps its own.
    pub fn portable_serial(id: &str) -> String {
        format!("portable-serial/{id}")
    }

    /// Data: the devices the root of `id` has given credentials to, JSON:
    /// the newest credential per device key, and its label.
    pub fn portable_issued(id: &str) -> String {
        format!("portable-issued/{id}")
    }

    /// Data: the newest identity state of `id` this device has, JSON:
    /// made here by its root, or brought from elsewhere.
    pub fn portable_state(id: &str) -> String {
        format!("portable-state/{id}")
    }

    /// Data: every device key of `id` revoked here, and when, JSON (key →
    /// Unix seconds): the union of all the states this device has made or
    /// accepted, older revisions too.
    pub fn portable_revoked(id: &str) -> String {
        format!("portable-revoked/{id}")
    }

    /// Data: the genesis of `id`, JSON: an identity begun with a
    /// security key root. A phrase identity has none.
    pub fn portable_genesis(id: &str) -> String {
        format!("portable-genesis/{id}")
    }

    /// Data: where and when guest `id` was last paired onto this device,
    /// JSON (the server the pairing was for, if one was known, and Unix
    /// seconds): what [`crate::Portable::guest_pairings`] lists. A guest
    /// with no pairing recorded has none.
    pub fn portable_paired(id: &str) -> String {
        format!("portable-paired/{id}")
    }

    /// Secret: the device key this device offered a pairing through a
    /// server with, before it knows the identity it will act as:
    /// taken as that identity's device key when the pairing is accepted.
    pub const PORTABLE_PENDING_DEVICE: &str = "portable-device-pending";

    /// What every key the app may sync between the user's devices starts
    /// with: an iOS app stores these, and only these, as
    /// synchronizable keychain items (iCloud Keychain), secrets and data
    /// alike. Every other key stays on this device, which is also what an
    /// app that does not know this prefix does: the safe default.
    pub const SYNCED_PREFIX: &str = "synced/";

    /// Whether the app may sync `key` between the user's devices: it
    /// starts with [`SYNCED_PREFIX`].
    pub fn is_synced(key: &str) -> bool {
        key.starts_with(SYNCED_PREFIX)
    }

    /// Secret, synced: a phrase identity's root signing key, 32 bytes, once
    /// the user turned on its sync ([`crate::Portable::enable_root_sync`]).
    /// The same bytes as [`portable_root`], under a key of its own, so
    /// nothing else of the identity's goes with it. Whoever's devices
    /// read it is that user everywhere: only the user's own, through
    /// their account's end-to-end encrypted keychain.
    pub fn portable_root_synced(id: &str) -> String {
        format!("{SYNCED_PREFIX}portable-root/{id}")
    }

    /// Data, synced: what every holder of a synced root needs of its
    /// identity, JSON: its name, every device key revoked (however long
    /// ago), the devices issued credentials, and its newest states. The
    /// SDK merges it with what is on this device whenever it reads or
    /// writes it, by union: a write from one device that overwrites
    /// another's (the keychain keeps the last) loses nothing for good,
    /// as the other device writes the union back the next time it merges
    /// ([`crate::Portable::merge_synced`]).
    pub fn portable_synced(id: &str) -> String {
        format!("{SYNCED_PREFIX}portable/{id}")
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
