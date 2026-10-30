//! The SDK's storage, implemented by the app: secrets in the platform's
//! secret store (the Keychain, Android's Keystore-backed storage), data in
//! a file. Synchronous on the foreign side, so each call runs on the
//! blocking pool: a Keychain prompt cannot stall networking or another
//! playback.
//!
//! Or [`FileStorage`]: files in a directory of the app's, its secrets
//! sealed by the app's [`SecretCipher`] (Android, whose Keystore holds a
//! key, not values).

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use spoolframe_client_sdk::storage::files;
use spoolframe_client_sdk::{Storage, StorageError};

/// The store couldn't answer. Never "empty": the SDK doesn't replace a key it
/// couldn't read.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum StorageFailure {
    #[error("{message}")]
    Failed { message: String },
}

/// Two key-value stores. `None` means the key isn't there; `set_*` with
/// `None` removes it.
#[uniffi::export(with_foreign)]
pub trait CoreStorage: Send + Sync {
    /// The device key and session tokens: the secret store.
    fn secret(&self, key: String) -> Result<Option<Vec<u8>>, StorageFailure>;
    fn set_secret(&self, key: String, value: Option<Vec<u8>>) -> Result<(), StorageFailure>;
    /// The saved servers: a file.
    fn data(&self, key: String) -> Result<Option<Vec<u8>>, StorageFailure>;
    fn set_data(&self, key: String, value: Option<Vec<u8>>) -> Result<(), StorageFailure>;
}

/// The app's storage as the SDK takes it.
pub(crate) struct Bridge(pub(crate) Arc<dyn CoreStorage>);

fn failed(StorageFailure::Failed { message }: StorageFailure) -> StorageError {
    StorageError(message)
}

#[async_trait]
impl Storage for Bridge {
    async fn secret(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        let store = self.0.clone();
        let key = key.to_owned();
        tokio::task::spawn_blocking(move || store.secret(key).map_err(failed))
            .await
            .map_err(|e| StorageError(format!("storage callback failed: {e}")))?
    }

    async fn set_secret(&self, key: &str, value: Option<Vec<u8>>) -> Result<(), StorageError> {
        let store = self.0.clone();
        let key = key.to_owned();
        tokio::task::spawn_blocking(move || store.set_secret(key, value).map_err(failed))
            .await
            .map_err(|e| StorageError(format!("storage callback failed: {e}")))?
    }

    async fn data(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        let store = self.0.clone();
        let key = key.to_owned();
        tokio::task::spawn_blocking(move || store.data(key).map_err(failed))
            .await
            .map_err(|e| StorageError(format!("storage callback failed: {e}")))?
    }

    async fn set_data(&self, key: &str, value: Option<Vec<u8>>) -> Result<(), StorageError> {
        let store = self.0.clone();
        let key = key.to_owned();
        tokio::task::spawn_blocking(move || store.set_data(key, value).map_err(failed))
            .await
            .map_err(|e| StorageError(format!("storage callback failed: {e}")))?
    }
}

/// Seals and opens the secrets a [`FileStorage`] keeps, with a key the
/// app holds (Android's Keystore): this side only ever sees them sealed.
#[uniffi::export(with_foreign)]
pub trait SecretCipher: Send + Sync {
    /// `plaintext`, sealed. `name` is the secret's key, bound to what is
    /// sealed, so one secret's file cannot stand in for another's.
    fn seal(&self, name: String, plaintext: Vec<u8>) -> Result<Vec<u8>, StorageFailure>;
    /// What `seal` sealed under `name`; an error for anything else.
    /// (Not `open`: a keyword in Swift that UniFFI's C header can't take.)
    fn unseal(&self, name: String, sealed: Vec<u8>) -> Result<Vec<u8>, StorageFailure>;
}

/// The SDK's storage as files under a directory: `data/` as the SDK
/// gives it, `secrets/` sealed by a [`SecretCipher`]. One file per key,
/// written whole and renamed into place, so a crash mid-write leaves the
/// old value, never half of the new one. Each rename and removal is made
/// durable before it returns (the directory synced): else a crash could
/// keep a later write in `data/` and lose an earlier one in `secrets/`,
/// and the SDK, finding no device key, would make a new one and orphan
/// the saved sign-ins. One `FileStorage` per directory.
#[derive(uniffi::Object)]
pub struct FileStorage {
    data: PathBuf,
    secrets: PathBuf,
    cipher: Arc<dyn SecretCipher>,
}

#[uniffi::export]
impl FileStorage {
    /// Storage under `root` (the app's private files directory), made if
    /// it is not there, its secrets sealed by `cipher`. The temporary
    /// files of writes a crash cut short are removed; this process's own,
    /// writes under way of another storage on `root` (an Android Activity
    /// made again while the old client still writes), are left.
    #[uniffi::constructor]
    pub fn new(root: String, cipher: Arc<dyn SecretCipher>) -> Result<Arc<Self>, StorageFailure> {
        let root = PathBuf::from(root);
        let data = root.join("data");
        let secrets = root.join("secrets");
        for dir in [&data, &secrets] {
            fs::create_dir_all(dir).map_err(|error| io_failed("making", dir, error))?;
            files::remove_temporaries(dir).map_err(|error| io_failed("clearing", dir, error))?;
        }
        files::sync_dir(&root).map_err(|error| io_failed("syncing", &root, error))?;
        Ok(Arc::new(Self {
            data,
            secrets,
            cipher,
        }))
    }
}

impl FileStorage {
    fn read(&self, dir: &Path, key: &str) -> Result<Option<Vec<u8>>, StorageFailure> {
        let path = dir.join(files::file_name(key));
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_failed("reading", &path, error)),
        }
    }

    fn write(&self, dir: &Path, key: &str, value: Option<&[u8]>) -> Result<(), StorageFailure> {
        let path = dir.join(files::file_name(key));
        match value {
            Some(value) => files::write(&path, value).map_err(|e| io_failed("writing", &path, e)),
            None => files::remove(&path).map_err(|e| io_failed("removing", &path, e)),
        }
    }
}

#[uniffi::export]
impl CoreStorage for FileStorage {
    fn secret(&self, key: String) -> Result<Option<Vec<u8>>, StorageFailure> {
        let Some(sealed) = self.read(&self.secrets, &key)? else {
            return Ok(None);
        };
        // A secret that does not open is an error, never "not there": the
        // SDK must not replace a key it cannot read.
        self.cipher
            .unseal(key.clone(), sealed)
            .map(Some)
            .map_err(|error| StorageFailure::Failed {
                message: format!("opening secret {key}: {error}"),
            })
    }

    fn set_secret(&self, key: String, value: Option<Vec<u8>>) -> Result<(), StorageFailure> {
        let sealed = value
            .map(|plain| self.cipher.seal(key.clone(), plain))
            .transpose()
            .map_err(|error| StorageFailure::Failed {
                message: format!("sealing secret {key}: {error}"),
            })?;
        self.write(&self.secrets, &key, sealed.as_deref())
    }

    fn data(&self, key: String) -> Result<Option<Vec<u8>>, StorageFailure> {
        self.read(&self.data, &key)
    }

    fn set_data(&self, key: String, value: Option<Vec<u8>>) -> Result<(), StorageFailure> {
        self.write(&self.data, &key, value.as_deref())
    }
}

fn io_failed(doing: &str, path: &Path, error: std::io::Error) -> StorageFailure {
    StorageFailure::Failed {
        message: format!("{doing} {}: {error}", path.display()),
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
