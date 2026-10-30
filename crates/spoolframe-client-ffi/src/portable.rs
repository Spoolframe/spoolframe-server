//! Portable identities: this phone as an identity's root device. The
//! root key stays in the SDK and the app's secret store; what crosses is
//! ids, fingerprints, names and, once, a new identity's phrase.

use std::sync::Arc;
use std::time::SystemTime;

use spoolframe_client_sdk as sdk;
use spoolframe_client_sdk::types::serde_time::unix_seconds;

use crate::error::{SpoolError, invalid};
use crate::runtime::run;

/// An identity this device holds the root of.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PortableIdentity {
    /// The id, 64 hex digits.
    pub id: String,
    /// `XXXX-XXXX-XXXX-XXXX`, as a server's fingerprint is shown.
    pub fingerprint: String,
    /// What the user called it here: its name as a guest on a server it joins.
    pub name: String,
    /// Made, or first recovered, here.
    pub created_at: SystemTime,
    /// Its root key is in this device's keychain. False for one a restored
    /// backup listed without its root: recover it from its phrase before
    /// it can authorize, revoke or tie.
    pub has_root: bool,
    /// Rooted in security keys (P7-08): its root actions take a key. A
    /// phrase identity that added a key isn't: its phrase is still a root.
    pub key_rooted: bool,
}

impl From<sdk::PortableIdentity> for PortableIdentity {
    fn from(entry: sdk::PortableIdentity) -> Self {
        Self {
            id: entry.id.to_string(),
            fingerprint: entry.id.fingerprint(),
            name: entry.name,
            created_at: entry.created_at,
            has_root: entry.has_root,
            key_rooted: entry.key_rooted,
        }
    }
}

/// A device this phone's root has authorized: its newest credential.
#[derive(Debug, Clone, uniffi::Record)]
pub struct IssuedDevice {
    /// The device's key, 64 hex digits.
    pub device_key: String,
    pub fingerprint: String,
    pub serial: u64,
    pub issued_at: SystemTime,
    pub expires_at: SystemTime,
    /// What the user called the device when authorizing it.
    pub label: Option<String>,
    /// None while it isn't revoked.
    pub revoked_at: Option<SystemTime>,
}

impl From<sdk::portable::IssuedCredential> for IssuedDevice {
    fn from(issued: sdk::portable::IssuedCredential) -> Self {
        Self {
            device_key: issued.device_key.to_string(),
            fingerprint: key_fingerprint(&issued.device_key),
            serial: issued.serial,
            issued_at: issued.issued_at,
            expires_at: issued.expires_at,
            label: issued.label,
            revoked_at: issued.revoked_at,
        }
    }
}

/// A guest paired onto this device.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Guest {
    pub id: String,
    pub fingerprint: String,
    /// The server it paired for, when known (paired before this was kept:
    /// `None`).
    pub server_name: Option<String>,
    pub server_endpoint_id: Option<String>,
    /// When.
    pub paired_at: Option<SystemTime>,
}

/// A new identity, with the phrase to show once.
#[derive(uniffi::Record)]
pub struct NewIdentity {
    pub identity: PortableIdentity,
    pub phrase: Arc<RecoveryPhrase>,
}

/// A new identity's 24 words. Cleared from the core's memory when the app
/// lets it go; there's no other way to read it again.
#[derive(uniffi::Object)]
pub struct RecoveryPhrase(sdk::RecoveryPhrase);

impl std::fmt::Debug for RecoveryPhrase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

#[uniffi::export]
impl RecoveryPhrase {
    /// The words, in order.
    pub fn words(&self) -> Vec<String> {
        self.0.words()
    }
}

/// A device key an identity's root has revoked, and since when.
#[derive(Debug, Clone, uniffi::Record)]
pub struct RevokedDevice {
    /// The key, 64 hex digits.
    pub key: String,
    pub fingerprint: String,
    pub revoked_at: SystemTime,
}

/// An identity's newest signed state on this device: what it revokes.
#[derive(Debug, Clone, uniffi::Record)]
pub struct IdentityState {
    pub revision: u64,
    pub revoked: Vec<RevokedDevice>,
    pub issued_at: SystemTime,
}

impl From<sdk::PortableIdentityState> for IdentityState {
    fn from(state: sdk::PortableIdentityState) -> Self {
        Self {
            revision: state.revision,
            revoked: state
                .revoked
                .iter()
                .map(|r| RevokedDevice {
                    key: r.key.to_string(),
                    fingerprint: key_fingerprint(&r.key),
                    revoked_at: unix_seconds::from_wire(r.revoked_at),
                })
                .collect(),
            issued_at: unix_seconds::from_wire(state.issued_at),
        }
    }
}

/// A device key's fingerprint, in the identity's format.
fn key_fingerprint(key: &sdk::DevicePublicKey) -> String {
    sdk::fingerprint::of_hex(&key.to_string()).unwrap_or_default()
}

pub(crate) fn identity(id: &str) -> Result<sdk::PortableUserId, SpoolError> {
    id.parse().map_err(|e| invalid("identity", e))
}

/// The portable identities on this device.
#[derive(uniffi::Object)]
pub struct SpoolPortable(pub(crate) sdk::Portable);

#[uniffi::export]
impl SpoolPortable {
    /// A new identity called `name`. Show its phrase once, and let it go.
    pub async fn create(&self, name: String) -> Result<NewIdentity, SpoolError> {
        let portable = self.0.clone();
        let (id, phrase) = run(async move { portable.create(&name).await }).await?;
        let identity = self.find(id).await?;
        Ok(NewIdentity {
            identity,
            phrase: Arc::new(RecoveryPhrase(phrase)),
        })
    }

    /// The identity `phrase` recovers, kept here as `name`; one already
    /// here keeps its name. A phrase that isn't one is `Phrase…`.
    pub async fn recover(
        &self,
        phrase: String,
        name: String,
    ) -> Result<PortableIdentity, SpoolError> {
        let portable = self.0.clone();
        let id = run(async move { portable.recover(&phrase, &name).await }).await?;
        self.find(id).await
    }

    /// The identities here, oldest first. A storage failure is an error,
    /// never an empty list.
    pub async fn list(&self) -> Result<Vec<PortableIdentity>, SpoolError> {
        let portable = self.0.clone();
        let list = run(async move { portable.list().await }).await?;
        Ok(list.into_iter().map(Into::into).collect())
    }

    /// Call identity `id` `name` on this device (trimmed, 1–64 characters):
    /// a server it joins later takes it as the guest's name; no root needed. A synced identity's
    /// name goes to the user's other devices, the newest rename winning.
    pub async fn rename(&self, id: String, name: String) -> Result<(), SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        Ok(run(async move { portable.rename(id, &name).await }).await?)
    }

    /// Remove identity `id` from this device; its phrase still recovers it.
    pub async fn forget(&self, id: String) -> Result<(), SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        Ok(run(async move { portable.forget(id).await }).await?)
    }

    /// This device's own key for identity `id`.
    pub async fn device_key(&self, id: String) -> Result<String, SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        Ok(run(async move { portable.device_key(id).await })
            .await?
            .to_string())
    }

    /// Revoke `device_key` for identity `id`, from now: the new state.
    pub async fn revoke(
        &self,
        id: String,
        device_key: String,
    ) -> Result<IdentityState, SpoolError> {
        let id = identity(&id)?;
        let key: sdk::DevicePublicKey = device_key.parse().map_err(|e| invalid("device key", e))?;
        let portable = self.0.clone();
        Ok(run(async move { portable.revoke(id, key).await })
            .await?
            .into())
    }

    /// The devices identity `id`'s root here has authorized, revoked ones
    /// too, in the order they were first authorized.
    pub async fn issued(&self, id: String) -> Result<Vec<IssuedDevice>, SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        let issued = run(async move { portable.issued(id).await }).await?;
        Ok(issued.into_iter().map(Into::into).collect())
    }

    /// Whether this device takes guests by QR pairing (P7-03); off until
    /// its owner turns it on.
    pub async fn allow_guests(&self) -> Result<bool, SpoolError> {
        let portable = self.0.clone();
        Ok(run(async move { portable.allow_guests().await }).await?)
    }

    pub async fn set_allow_guests(&self, allowed: bool) -> Result<(), SpoolError> {
        let portable = self.0.clone();
        Ok(run(async move { portable.set_allow_guests(allowed).await }).await?)
    }

    /// The guests paired onto this device: their identities, the server
    /// each paired for and when.
    pub async fn guests(&self) -> Result<Vec<Guest>, SpoolError> {
        let portable = self.0.clone();
        let guests = run(async move { portable.guest_pairings().await }).await?;
        Ok(guests
            .into_iter()
            .map(|g| Guest {
                id: g.id.to_string(),
                fingerprint: g.id.fingerprint(),
                server_name: g.server.as_ref().map(|s| s.name.clone()),
                server_endpoint_id: g.server.map(|s| s.endpoint_id),
                paired_at: g.paired_at,
            })
            .collect())
    }

    /// Sign guest `id` out of this device: its credential and key go.
    pub async fn forget_guest(&self, id: String) -> Result<(), SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        Ok(run(async move { portable.forget_guest(id).await }).await?)
    }

    /// Identity `id`'s newest state here; none when nothing is revoked.
    pub async fn identity_state(&self, id: String) -> Result<Option<IdentityState>, SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        Ok(run(async move { portable.identity_state(id).await })
            .await?
            .map(Into::into))
    }
}

impl SpoolPortable {
    async fn find(&self, id: sdk::PortableUserId) -> Result<PortableIdentity, SpoolError> {
        self.list()
            .await?
            .into_iter()
            .find(|entry| entry.id == id.to_string())
            .ok_or_else(|| SpoolError::UnknownIdentity { id: id.to_string() })
    }
}

/// A portable id's fingerprint (`XXXX-XXXX-XXXX-XXXX`); none for text
/// that isn't an id.
#[uniffi::export]
pub fn portable_fingerprint(id: String) -> Option<String> {
    id.parse::<sdk::PortableUserId>()
        .ok()
        .map(|id| id.fingerprint())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frozen derivation: 23 × "abandon" + "art".
    #[tokio::test]
    async fn the_test_vector_recovers_its_id() {
        let portable = SpoolPortable(sdk::Portable::new(Arc::new(sdk::MemoryStorage::default())));
        let phrase = format!("{}art", "abandon ".repeat(23));
        let id = portable.recover(phrase, "test".into()).await.unwrap();
        assert_eq!(
            id.id,
            "610b431103deb4f932b5a063abf7d0f5d3fdcb63a6ae542283d6d4e0fd737111"
        );
        assert_eq!(portable_fingerprint(id.id.clone()), Some(id.fingerprint));
    }

    #[tokio::test]
    async fn a_misspelt_word_names_its_position() {
        let portable = SpoolPortable(sdk::Portable::new(Arc::new(sdk::MemoryStorage::default())));
        let phrase = format!("{}abandn art", "abandon ".repeat(22));
        let error = portable.recover(phrase, "test".into()).await.unwrap_err();
        assert_eq!(
            error,
            SpoolError::Phrase {
                error: crate::error::PhraseFailure::UnknownWord { position: 23 }
            }
        );
    }
}
