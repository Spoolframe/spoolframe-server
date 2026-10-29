//! The SDK's storage, implemented by the app: secrets in the platform's
//! secret store (the Keychain, Android's Keystore-backed storage), data in
//! a file. Synchronous on the foreign side, so each call runs on the
//! blocking pool: a Keychain prompt cannot stall networking or another
//! playback.

use std::sync::Arc;

use async_trait::async_trait;
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
