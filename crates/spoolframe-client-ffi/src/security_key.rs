//! Security keys as identity roots, reached through the platform's API:
//! Apple's `ASAuthorizationSecurityKeyPublicKeyCredentialProvider` (NFC,
//! USB-C or Lightning on iPhone and iPad, USB on the Mac), Android's
//! Credential Manager. The app implements [`AppSecurityKey`]; the SDK sees
//! it as its `SecurityKey`, answering in WebAuthn form. The key's PIN is
//! the system's to ask.

use std::sync::Arc;

use async_trait::async_trait;
use spoolframe_client_sdk as sdk;

use crate::client::SpoolClient;
use crate::error::{SpoolError, invalid};
use crate::portable::{IdentityState, SpoolPortable, identity};
use crate::runtime::run;

/// Why the platform's key ceremony didn't finish. Apple says no more than
/// this: a wrong, locked or missing PIN all come back as `Failed`.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum KeyFailure {
    #[error("{message}")]
    Failed { message: String },
    #[error("cancelled")]
    Cancelled,
}

/// A WebAuthn registration, as the platform returned it.
#[derive(Debug, Clone, uniffi::Record)]
pub struct KeyRegistration {
    pub attestation_object: Vec<u8>,
    /// The exact `rawClientDataJSON`.
    pub client_data_json: Vec<u8>,
}

/// A WebAuthn assertion, as the platform returned it.
#[derive(Debug, Clone, uniffi::Record)]
pub struct KeyAnswer {
    pub credential_id: Vec<u8>,
    pub authenticator_data: Vec<u8>,
    /// The exact `rawClientDataJSON`.
    pub client_data_json: Vec<u8>,
    /// DER.
    pub signature: Vec<u8>,
    /// The credential's `user.id`: the identity's handle, 32 bytes.
    pub user_handle: Option<Vec<u8>>,
}

/// A security key, as the app reaches it through the platform.
#[uniffi::export(with_foreign)]
#[async_trait]
pub trait AppSecurityKey: Send + Sync {
    /// Make a resident ES256 credential under `rp` for user id `handle`
    /// (32 bytes), shown as `name`: challenge = `handle`, user
    /// verification required.
    async fn make_credential(
        &self,
        rp: String,
        handle: Vec<u8>,
        name: String,
    ) -> Result<KeyRegistration, KeyFailure>;
    /// Assert under `rp` with `challenge` (32 bytes, as they are) and the
    /// credentials in `allowed`, or any discoverable one when empty; user
    /// verification required.
    async fn get_assertion(
        &self,
        rp: String,
        challenge: Vec<u8>,
        allowed: Vec<Vec<u8>>,
    ) -> Result<Vec<KeyAnswer>, KeyFailure>;
}

/// The app's key as the SDK takes it.
pub(crate) struct Adapter(pub(crate) Arc<dyn AppSecurityKey>);

fn failed(e: KeyFailure) -> sdk::SdkError {
    match e {
        KeyFailure::Cancelled => sdk::SecurityKeyError::Cancelled.into(),
        KeyFailure::Failed { message } => sdk::SecurityKeyError::Failed(Some(message)).into(),
    }
}

#[async_trait]
impl sdk::SecurityKey for Adapter {
    async fn register_root(
        &self,
        rp: &str,
        handle: [u8; sdk::HANDLE_LEN],
        name: &str,
    ) -> Result<sdk::RootRegistration, sdk::SdkError> {
        let answer = self
            .0
            .make_credential(rp.to_owned(), handle.to_vec(), name.to_owned())
            .await
            .map_err(failed)?;
        Ok(sdk::RootRegistration::WebAuthn {
            attestation_object: answer.attestation_object,
            client_data_json: answer.client_data_json,
        })
    }

    async fn assert(
        &self,
        rp: &str,
        client_data_hash: [u8; 32],
        allowed: &[Vec<u8>],
    ) -> Result<Vec<sdk::KeyAssertion>, sdk::SdkError> {
        let answers = self
            .0
            .get_assertion(rp.to_owned(), client_data_hash.to_vec(), allowed.to_vec())
            .await
            .map_err(failed)?;
        if answers.is_empty() {
            return Err(sdk::SecurityKeyError::NoCredential.into());
        }
        Ok(answers
            .into_iter()
            .map(|a| sdk::KeyAssertion {
                credential_id: a.credential_id,
                signature: sdk::RootSignature::WebAuthn {
                    authenticator_data: a.authenticator_data,
                    client_data_json: a.client_data_json,
                    signature: a.signature,
                },
                user_handle: a.user_handle.and_then(|h| h.try_into().ok()),
            })
            .collect())
    }
}

/// One of an identity's roots now.
#[derive(Debug, Clone, uniffi::Record)]
pub struct IdentityRoot {
    /// None for the phrase root; a security key's credential id.
    pub credential_id: Option<Vec<u8>>,
    /// `XXXX-XXXX` from the key's credential, to tell keys apart.
    pub label: String,
}

pub(crate) fn root_of(root: &sdk::RootKey) -> IdentityRoot {
    match root {
        sdk::RootKey::Phrase(_) => IdentityRoot {
            credential_id: None,
            label: "Recovery phrase".into(),
        },
        sdk::RootKey::SecurityKey { credential_id, .. } => {
            let hex: String = credential_id
                .iter()
                .take(4)
                .map(|b| format!("{b:02X}"))
                .collect();
            IdentityRoot {
                credential_id: Some(credential_id.clone()),
                label: format!("{}-{}", &hex[..hex.len().min(4)], &hex[hex.len().min(4)..]),
            }
        }
    }
}

#[uniffi::export]
impl SpoolPortable {
    /// A new identity called `name`, rooted in `key` alone (P7-08): asked
    /// twice, a touch and the PIN each time. No phrase.
    pub async fn create_with_key(
        &self,
        key: Arc<dyn AppSecurityKey>,
        name: String,
    ) -> Result<String, SpoolError> {
        let portable = self.0.clone();
        let id = run(async move {
            let key = Adapter(key);
            portable.create_with_keys(&[&key], &name).await
        })
        .await?;
        Ok(id.to_string())
    }

    /// Identity `id`'s roots now: its phrase (if still one), its keys.
    pub async fn roots(&self, id: String) -> Result<Vec<IdentityRoot>, SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        let roots = run(async move { portable.roots(id).await }).await?;
        Ok(roots.members.iter().map(root_of).collect())
    }

    /// Add `new_key` as a root of `id`, signed by `signer` (a key that is
    /// a root now), or by the phrase root here when `None`.
    pub async fn add_root(
        &self,
        id: String,
        signer: Option<Arc<dyn AppSecurityKey>>,
        new_key: Arc<dyn AppSecurityKey>,
        name: String,
    ) -> Result<IdentityState, SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        let state = run(async move {
            let new_key = Adapter(new_key);
            let signer = signer.map(Adapter);
            let by = signer
                .as_ref()
                .map_or(sdk::RootSigner::Phrase, |k| sdk::RootSigner::Key(k));
            portable.add_root(id, by, &new_key, &name).await
        })
        .await?;
        Ok(state.into())
    }

    /// Remove a root of `id`: the key with `credential_id`, or the phrase
    /// root when `None`; signed by `signer` or the phrase root here.
    pub async fn remove_root(
        &self,
        id: String,
        credential_id: Option<Vec<u8>>,
        signer: Option<Arc<dyn AppSecurityKey>>,
    ) -> Result<IdentityState, SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        let state = run(async move {
            let roots = portable.roots(id).await?;
            let root = roots
                .members
                .iter()
                .find(|r| match (r, &credential_id) {
                    (sdk::RootKey::Phrase(_), None) => true,
                    (
                        sdk::RootKey::SecurityKey {
                            credential_id: c, ..
                        },
                        Some(wanted),
                    ) => c == wanted,
                    _ => false,
                })
                .cloned()
                .ok_or(sdk::SdkError::NotARoot)?;
            let signer = signer.map(Adapter);
            let by = signer
                .as_ref()
                .map_or(sdk::RootSigner::Phrase, |k| sdk::RootSigner::Key(k));
            portable.remove_root(id, by, &root).await
        })
        .await?;
        Ok(state.into())
    }

    /// Revoke `device_key` for `id`, signed by `signer` or the phrase root.
    pub async fn revoke_by(
        &self,
        id: String,
        device_key: String,
        signer: Option<Arc<dyn AppSecurityKey>>,
    ) -> Result<IdentityState, SpoolError> {
        let id = identity(&id)?;
        let key: sdk::DevicePublicKey = device_key.parse().map_err(|e| invalid("device key", e))?;
        let portable = self.0.clone();
        let state = run(async move {
            let signer = signer.map(Adapter);
            let by = signer
                .as_ref()
                .map_or(sdk::RootSigner::Phrase, |k| sdk::RootSigner::Key(k));
            portable.revoke_by(id, by, key).await
        })
        .await?;
        Ok(state.into())
    }

    /// Give this device a credential for `id` with `key`, one of its roots:
    /// how a device with only the key acts as the identity, and how it
    /// renews (one touch).
    pub async fn pair_self(
        &self,
        id: String,
        key: Arc<dyn AppSecurityKey>,
    ) -> Result<(), SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        run(async move {
            let key = Adapter(key);
            portable.pair_self(id, sdk::RootSigner::Key(&key)).await
        })
        .await?;
        Ok(())
    }

    /// When this device's credential for `id` expires, Unix seconds, if
    /// it's due for renewal (its last 14 days, or past).
    pub async fn renewal_due(&self, id: String) -> Result<Option<i64>, SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        Ok(run(async move { portable.renewal_due(id).await }).await?)
    }
}

#[uniffi::export]
impl SpoolClient {
    /// Recover an identity on this device with `key` alone, from server
    /// `endpoint_id`, which knows it (P7-08), as `name`: its id. Then
    /// `pair_self` with the same key gives this device a credential.
    pub async fn recover_with_key(
        &self,
        endpoint_id: String,
        key: Arc<dyn AppSecurityKey>,
        name: String,
    ) -> Result<String, SpoolError> {
        let client = self.client.clone();
        let id = run(async move {
            let key = Adapter(key);
            client.recover_with_key(&endpoint_id, &key, &name).await
        })
        .await?;
        Ok(id.to_string())
    }
}
