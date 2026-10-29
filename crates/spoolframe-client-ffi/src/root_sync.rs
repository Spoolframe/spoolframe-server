//! A phrase root synced between the user's own devices: opt-in per
//! identity, only with a security key root too. The app syncs exactly the
//! storage keys that start with `synced/` (iCloud Keychain on Apple); the
//! SDK merges what arrives.

use std::sync::Arc;

use spoolframe_client_sdk::{self as sdk, RootLocation};

use crate::error::SpoolError;
use crate::portable::{SpoolPortable, identity};
use crate::runtime::run;
use crate::security_key::{Adapter, AppSecurityKey};

/// Whether storage key `key` is one the app syncs.
#[uniffi::export]
pub fn is_synced_key(key: String) -> bool {
    sdk::storage::keys::is_synced(&key)
}

/// The identity a synced root's storage key names, if it is one.
#[uniffi::export]
pub fn synced_root_identity(key: String) -> Option<String> {
    let prefix = sdk::storage::keys::portable_root_synced("");
    key.strip_prefix(&prefix)
        .filter(|id| id.parse::<sdk::PortableUserId>().is_ok())
        .map(str::to_owned)
}

#[uniffi::export]
impl SpoolPortable {
    pub async fn root_location(&self, id: String) -> Result<RootLocation, SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        Ok(run(async move { portable.root_location(id).await }).await?)
    }

    /// Turn on the sync of `id`'s phrase root. Refused for a key identity
    /// (`RootSync { KeyRooted }`), and without a security key root
    /// (`RootSync { NoSecurityKey }`).
    pub async fn enable_root_sync(&self, id: String) -> Result<(), SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        Ok(run(async move { portable.enable_root_sync(id).await }).await?)
    }

    /// Keep a copy of `id`'s synced root as this device's own, so turning
    /// sync off anywhere leaves it here.
    pub async fn keep_root_on_device(&self, id: String) -> Result<(), SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        Ok(run(async move { portable.keep_root_on_device(id).await }).await?)
    }

    /// Turn off the sync of `id`'s root: a copy stays here; the synced one
    /// goes, on every device.
    pub async fn disable_root_sync(&self, id: String) -> Result<(), SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        Ok(run(async move { portable.disable_root_sync(id).await }).await?)
    }

    /// List `id`, whose synced root arrived here, as `name`.
    pub async fn adopt_synced(&self, id: String, name: String) -> Result<(), SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        run(async move { portable.adopt_synced(id, &name).await }).await?;
        Ok(())
    }

    /// Merge `id`'s synced record with this device's, both ways: at launch,
    /// and when a synced item changes. Whether anything here changed.
    pub async fn merge_synced(&self, id: String) -> Result<bool, SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        Ok(run(async move { portable.merge_synced(id).await }).await?)
    }

    /// With rival states here, sign one over them with `signer` (a key),
    /// or the phrase root when `None`. Whether one was signed.
    pub async fn reconcile(
        &self,
        id: String,
        signer: Option<Arc<dyn AppSecurityKey>>,
    ) -> Result<bool, SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        let state = run(async move {
            let signer = signer.map(Adapter);
            let by = signer
                .as_ref()
                .map_or(sdk::RootSigner::Phrase, |k| sdk::RootSigner::Key(k));
            portable.reconcile(id, by).await
        })
        .await?;
        Ok(state.is_some())
    }

    /// How many rival states of `id` this device holds (2 means a merge
    /// is due).
    pub async fn head_count(&self, id: String) -> Result<u32, SpoolError> {
        let id = identity(&id)?;
        let portable = self.0.clone();
        Ok(run(async move { portable.identity_states(id).await })
            .await?
            .len() as u32)
    }
}
