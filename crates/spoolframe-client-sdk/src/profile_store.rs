//! What an app keeps for each profile between starts: its play queue,
//! its track choices, its recent searches. In the app's plain data
//! ([`Storage::data`]), never its secrets: none of it is one, and a
//! keyring is no place for a queue.
//!
//! A profile's values are named by a hash of its server and username
//! ([`profile_hash`]): a username need not fit a key or a file name. It is
//! the Linux app's own `sha256("<server>/<username>")`, so its files
//! (`queues/<hash>.json`, `searches/<hash>.json`) move over as they are.

use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::storage::Storage;
use crate::{Client, SdkError};

/// The hash a profile's values are kept under: SHA-256 of
/// `"<server>/<username>"`, in lowercase hex. `server` is the hex endpoint
/// id.
pub fn profile_hash(server: &str, username: &str) -> String {
    Sha256::digest(format!("{server}/{username}"))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// One profile's values ([`Client::profile_store`]). Cheap to clone.
#[derive(Clone)]
pub struct ProfileStore {
    storage: Arc<dyn Storage>,
    hash: String,
}

impl std::fmt::Debug for ProfileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileStore")
            .field("hash", &self.hash)
            .finish_non_exhaustive()
    }
}

/// A value's name: lowercase ASCII letters, digits, `-` and `_`, 1 to 64
/// of them, so it reads the same as a key or a file name anywhere.
fn checked(name: &str) -> Result<&str, SdkError> {
    let fits = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    fits.then_some(name).ok_or_else(|| {
        SdkError::InvalidInput(format!(
            "{name:?} is no value's name: lowercase letters, digits, - and _"
        ))
    })
}

impl ProfileStore {
    /// The storage key of value `name`: `profile/<hash>/<name>`.
    fn key(&self, name: &str) -> Result<String, SdkError> {
        Ok(format!("profile/{}/{}", self.hash, checked(name)?))
    }

    /// Value `name`, as it was set; `None` if it never was.
    pub async fn get(&self, name: &str) -> Result<Option<Vec<u8>>, SdkError> {
        Ok(self.storage.data(&self.key(name)?).await?)
    }

    /// Set value `name`; `None` removes it.
    pub async fn set(&self, name: &str, value: Option<Vec<u8>>) -> Result<(), SdkError> {
        Ok(self.storage.set_data(&self.key(name)?, value).await?)
    }

    /// Value `name` read as JSON; `None` if it never was set, or no longer
    /// reads as a `T` (an older app's): a value kept is not worth failing
    /// for.
    pub async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        name: &str,
    ) -> Result<Option<T>, SdkError> {
        Ok(self.get(name).await?.and_then(|bytes| {
            serde_json::from_slice(&bytes)
                .inspect_err(
                    |e| tracing::warn!(name, error = %e, "a profile's value cannot be read"),
                )
                .ok()
        }))
    }

    /// Set value `name` to `value` as JSON.
    pub async fn set_json<T: serde::Serialize>(
        &self,
        name: &str,
        value: &T,
    ) -> Result<(), SdkError> {
        let bytes = serde_json::to_vec(value)
            .map_err(|e| SdkError::InvalidInput(format!("{name} does not serialize: {e}")))?;
        self.set(name, Some(bytes)).await
    }
}

impl Client {
    /// What the app keeps for `username` on server `endpoint_id`, by name.
    /// Nothing is read until asked.
    pub fn profile_store(&self, endpoint_id: &str, username: &str) -> ProfileStore {
        ProfileStore {
            storage: self.inner.storage.clone(),
            hash: profile_hash(endpoint_id, username),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStorage;

    fn store(username: &str) -> ProfileStore {
        ProfileStore {
            storage: Arc::new(MemoryStorage::default()),
            hash: profile_hash("ab", username),
        }
    }

    /// The Linux app's hash, `glib::compute_checksum_for_string(Sha256,
    /// "<server>/<username>")` (`printf server/alice | sha256sum`): its
    /// files move over as they are.
    #[test]
    fn the_hash_is_the_linux_apps() {
        assert_eq!(
            profile_hash("server", "alice"),
            "c793d7683c8e69e36999e9efbc74f3264ea65c407c336b37502882e3a8785d58"
        );
    }

    #[tokio::test]
    async fn a_value_reads_back_as_it_was_set_and_goes() {
        let store = store("alice");
        store.set("queue", Some(b"[1]".to_vec())).await.unwrap();
        assert_eq!(store.get("queue").await.unwrap(), Some(b"[1]".to_vec()));
        store.set("queue", None).await.unwrap();
        assert_eq!(store.get("queue").await.unwrap(), None);
    }

    #[tokio::test]
    async fn two_profiles_keep_their_own() {
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::default());
        let of = |username: &str| ProfileStore {
            storage: storage.clone(),
            hash: profile_hash("ab", username),
        };
        of("alice").set_json("searches", &["dune"]).await.unwrap();
        assert_eq!(
            of("bob").get_json::<Vec<String>>("searches").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_name_that_is_no_key_is_refused() {
        assert!(matches!(
            store("alice").get("../servers").await,
            Err(SdkError::InvalidInput(_))
        ));
    }
}
