//! Syncing a phrase identity's root between the user's own devices
//! (2026-10-12): opt-in per identity, off unless the user turns it on, and
//! only for a phrase root. The app does the syncing (iCloud Keychain on
//! Apple platforms); the SDK gives it the keys to sync and merges what
//! arrives. The SDK itself never sends a root anywhere, nor logs one.
//!
//! **What the app syncs.** Every key that starts with
//! [`keys::SYNCED_PREFIX`] ([`keys::is_synced`]), and nothing else:
//!
//! - [`keys::portable_root_synced`]: secret, the root's 32 bytes;
//! - [`keys::portable_synced`]: data, a record of the identity: the name
//!   the user gave it, with when it was last renamed (it goes to their
//!   own devices only, never to a server), every device key revoked
//!   (however long ago), the devices issued credentials, and its newest
//!   states.
//!
//! A keychain keeps the last write of an item, so the record is never
//! taken as it comes: the SDK merges it with what is on this device, by
//! union (a revoked key at its earliest, a device at its newest
//! credential, states by [`super::heads`]; the name, the newest rename),
//! and writes the union back to both. A write from one device that overwrites another's loses nothing
//! for good: the other writes the union back the next time it merges.
//! It merges before and after every root action, and on
//! [`Portable::merge_synced`], which the app calls when the keychain says
//! a synced item changed, and at launch.
//!
//! **Why the revoked keys travel.** Servers and states forget a
//! revocation once no credential it could stop still holds; what stops a
//! root issuing to that key again is the revoked keys its holder keeps.
//! A holder that had only the root would un-revoke it.
//!
//! **The flow.**
//!
//! 1. On: [`Portable::enable_root_sync`] on the device that has the root.
//!    Refused for a security key identity (nothing to sync), a phrase
//!    root the identity removed, and an identity with no security key
//!    root that is still one: with one, a lost synced device is shut out
//!    by removing the phrase root with the key, not by starting over.
//! 2. On every other device, once the synced items arrive:
//!    [`Portable::root_location`] is [`RootLocation::Synced`] and
//!    [`Portable::adopt_synced`] lists the identity.
//! 3. Off: [`Portable::keep_root_on_device`] on **every** device first,
//!    then [`Portable::disable_root_sync`] on one, which deletes the
//!    synced items (and so, through the keychain, every device's synced
//!    copy). Each device keeps its own.
//!
//! [`Portable::forget`] removes this device's data only, never the synced
//! items: the user's other devices keep the identity, and this one can
//! adopt it again.
//!
//! [`Portable`]: super::Portable
//! [`Portable::forget`]: super::Portable::forget

use serde::{Deserialize, Serialize};

#[cfg(doc)]
use super::Portable;
use super::heads::{load_heads, save_heads, take_in};
use super::{
    IssuedCredential, PortableIdentityState, PortableRoot, PortableUserId, Revoked, RootKey, load,
    load_issued, load_revoked, save, save_issued, save_revoked, unix_now,
};
use crate::error::SdkError;
use crate::storage::{Storage, StorageError, keys};

/// Where identity `id`'s root is on this device ([`Portable::root_location`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootLocation {
    /// Not here: a guest, a security key identity, or one forgotten.
    None,
    /// This device's own copy ([`keys::portable_root`]), not synced.
    ThisDevice,
    /// The synced copy only ([`keys::portable_root_synced`]).
    Synced,
    /// Both: sync is on, and this device kept a copy of its own
    /// ([`Portable::keep_root_on_device`]), to turn it off.
    ThisDeviceAndSynced,
}

/// Why turning on a root's sync was refused ([`Portable::enable_root_sync`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RootSyncRefused {
    /// The identity is rooted in security keys: it has no secret to sync,
    /// and a key acts for it on any device as it is.
    #[error("an identity rooted in security keys has no root secret to sync")]
    KeyRooted,
    /// The identity has no security key root that is still one. With one,
    /// a lost synced device is shut out by removing the phrase root with
    /// the key; without, only a new identity would do.
    #[error("add a security key as a root of the identity before syncing its phrase root")]
    NoSecurityKey,
}

/// What [`keys::portable_synced`] holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SyncRecord {
    name: String,
    /// When `name` was set by a rename ([`super::PortableIdentity::name_set_at`]).
    // compat(v1): required at the protocol reset
    #[serde(default)]
    name_set_at: u64,
    created_at: u64,
    #[serde(default)]
    revoked: Revoked,
    #[serde(default)]
    issued: Vec<IssuedCredential>,
    #[serde(default)]
    heads: Vec<PortableIdentityState>,
}

impl super::Portable {
    /// Where identity `id`'s root is on this device: its own copy, the
    /// synced one, both, or neither. A synced root of an identity not
    /// listed here is [`RootLocation::Synced`]: [`Self::adopt_synced`]
    /// lists it.
    pub async fn root_location(&self, id: PortableUserId) -> Result<RootLocation, SdkError> {
        let storage = self.storage.as_ref();
        let here = storage
            .secret(&keys::portable_root(&id.to_string()))
            .await?
            .is_some();
        let synced = storage
            .secret(&keys::portable_root_synced(&id.to_string()))
            .await?
            .is_some();
        Ok(match (here, synced) {
            (false, false) => RootLocation::None,
            (true, false) => RootLocation::ThisDevice,
            (false, true) => RootLocation::Synced,
            (true, true) => RootLocation::ThisDeviceAndSynced,
        })
    }

    /// Whether identity `id`'s root is on this device, its own copy or
    /// the synced one.
    pub async fn has_root(&self, id: PortableUserId) -> Result<bool, SdkError> {
        Ok(self.root_location(id).await? != RootLocation::None)
    }

    /// Turn on the sync of identity `id`'s phrase root: it is written to
    /// [`keys::portable_root_synced`], with the identity's record
    /// ([`keys::portable_synced`]), and then removed from this device's
    /// own key. Ask the user first: every device the app syncs to is
    /// the identity from then on. Refused with
    /// [`RootSyncRefused::KeyRooted`] for a security key identity,
    /// [`SdkError::NotARoot`] for a phrase root the identity removed,
    /// [`RootSyncRefused::NoSecurityKey`] for an identity with no security
    /// key root that is still one, and [`SdkError::UnknownIdentity`]
    /// without the root here. On already, it changes nothing.
    pub async fn enable_root_sync(&self, id: PortableUserId) -> Result<(), SdkError> {
        let storage = self.storage.as_ref();
        let entry = load(storage)
            .await?
            .into_iter()
            .find(|e| e.id == id)
            .ok_or_else(|| SdkError::UnknownIdentity(id.to_string()))?;
        if entry.key_rooted {
            return Err(RootSyncRefused::KeyRooted.into());
        }
        let root = self.load_root(id).await?;
        let roots = self.roots(id).await?;
        if !roots.holds(&RootKey::phrase_of(&id)) {
            return Err(SdkError::NotARoot);
        }
        if !roots
            .members
            .iter()
            .any(|r| matches!(r, RootKey::SecurityKey { .. }))
        {
            return Err(RootSyncRefused::NoSecurityKey.into());
        }
        // The synced copy first: should the rest fail, this device's own
        // is still there.
        storage
            .set_secret(
                &keys::portable_root_synced(&id.to_string()),
                Some(root.secret().to_vec()),
            )
            .await?;
        self.sync_data(id).await?;
        storage
            .set_secret(&keys::portable_root(&id.to_string()), None)
            .await?;
        tracing::info!(%id, "turned on the sync of a portable identity's root");
        Ok(())
    }

    /// Keep a copy of identity `id`'s synced root as this device's own
    /// ([`keys::portable_root`]), checked to be `id`'s. Run it on every
    /// device before [`Self::disable_root_sync`], so each keeps the root
    /// once the synced one is gone. With no synced root, nothing to do if
    /// this device has its own; else [`SdkError::UnknownIdentity`].
    pub async fn keep_root_on_device(&self, id: PortableUserId) -> Result<(), SdkError> {
        let storage = self.storage.as_ref();
        let Some(root) = load_synced_root(storage, id).await? else {
            return match storage
                .secret(&keys::portable_root(&id.to_string()))
                .await?
            {
                Some(_) => Ok(()),
                None => Err(SdkError::UnknownIdentity(id.to_string())),
            };
        };
        storage
            .set_secret(
                &keys::portable_root(&id.to_string()),
                Some(root.secret().to_vec()),
            )
            .await?;
        Ok(())
    }

    /// Turn off the sync of identity `id`'s root: keep a copy here
    /// ([`Self::keep_root_on_device`]), then delete the synced root and
    /// record. Through the keychain that deletes them on every device:
    /// run [`Self::keep_root_on_device`] on each of them first, or they
    /// lose the root (the phrase, or a security key, still recovers it).
    pub async fn disable_root_sync(&self, id: PortableUserId) -> Result<(), SdkError> {
        self.keep_root_on_device(id).await?;
        // What the record held is on this device too: merged into it.
        self.sync_data(id).await?;
        let storage = self.storage.as_ref();
        storage
            .set_secret(&keys::portable_root_synced(&id.to_string()), None)
            .await?;
        storage
            .set_data(&keys::portable_synced(&id.to_string()), None)
            .await?;
        tracing::info!(%id, "turned off the sync of a portable identity's root");
        Ok(())
    }

    /// List identity `id`, whose synced root has arrived on this device
    /// ([`RootLocation::Synced`]), as `name`, and take in its synced
    /// record: its revoked keys, devices and states. The root is checked
    /// to be `id`'s first; another's is [`crate::StorageError`], and
    /// nothing is listed. No synced root is [`SdkError::UnknownIdentity`].
    /// An identity listed already keeps its name.
    pub async fn adopt_synced(
        &self,
        id: PortableUserId,
        name: &str,
    ) -> Result<PortableUserId, SdkError> {
        if load_synced_root(self.storage.as_ref(), id).await?.is_none() {
            return Err(SdkError::UnknownIdentity(id.to_string()));
        }
        self.keep_entry(id, name, false).await?;
        self.merge_synced(id).await?;
        tracing::info!(%id, "adopted a synced portable identity");
        Ok(id)
    }

    /// Merge identity `id`'s synced record with what is on this device,
    /// both ways, by union (never the last writer's alone), and with the
    /// phrase root here, sign one state over rival heads. Call it when
    /// the keychain says a synced item changed, and at launch. Whether
    /// anything on this device changed. Sync off, it does nothing.
    pub async fn merge_synced(&self, id: PortableUserId) -> Result<bool, SdkError> {
        let changed = self.sync_data(id).await?;
        if load_heads(self.storage.as_ref(), id).await?.len() > 1 {
            self.merge_heads_if_root_here(id).await?;
        }
        Ok(changed)
    }

    /// [`Self::merge_synced`] without signing: what every root action does
    /// before (to act on what the other holders did) and after (to hand
    /// on what it did). Whether anything on this device changed. Nothing
    /// for an identity whose root is not synced, or not listed here.
    pub(super) async fn sync_data(&self, id: PortableUserId) -> Result<bool, SdkError> {
        let storage = self.storage.as_ref();
        if storage
            .secret(&keys::portable_root_synced(&id.to_string()))
            .await?
            .is_none()
        {
            return Ok(false);
        }
        let _saving = self.saving.lock().await;
        let mut saved = load(storage).await?;
        let Some(entry) = saved.iter_mut().find(|e| e.id == id) else {
            return Ok(false);
        };
        let remote = load_synced(storage, id).await?;
        // The newest rename wins: a newer one in the record is listed
        // here, and a newer one here goes in the record. On a tie the
        // record keeps its name and this device its own, as before names
        // had a time: a name made, recovered or adopted with is 0.
        let renamed = match &remote {
            Some(r) if r.name_set_at > entry.name_set_at => {
                entry.name.clone_from(&r.name);
                entry.name_set_at = r.name_set_at;
                true
            }
            _ => false,
        };
        let (name, name_set_at) = match &remote {
            Some(r) if r.name_set_at >= entry.name_set_at => (r.name.clone(), r.name_set_at),
            _ => (entry.name.clone(), entry.name_set_at),
        };
        let created_at = remote.as_ref().map_or(entry.created_at, |r| r.created_at);
        if renamed {
            save(storage, &saved).await?;
        }
        let mut revoked = load_revoked(storage, id).await?;
        let mut issued = load_issued(storage, id).await?;
        let mut heads = load_heads(storage, id).await?;
        let mut changed = (false, false, false);
        if let Some(remote) = &remote {
            changed.0 = merge_revoked(&mut revoked, &remote.revoked);
            changed.1 = merge_issued(&mut issued, &remote.issued);
            for state in &remote.heads {
                if !self.believes(id, &heads, state).await? {
                    continue;
                }
                if let Some(taken) = take_in(&heads, state, unix_now()) {
                    heads = taken;
                    changed.2 = true;
                }
            }
        }
        for head in &heads {
            changed.0 |= super::merge(&mut revoked, &head.revoked);
        }
        if changed.0 {
            save_revoked(storage, id, &revoked).await?;
        }
        if changed.1 {
            save_issued(storage, id, &issued).await?;
        }
        if changed.2 {
            save_heads(storage, id, &heads).await?;
        }
        let record = SyncRecord {
            name,
            name_set_at,
            created_at,
            revoked,
            issued,
            heads,
        };
        if remote.as_ref() != Some(&record) {
            let bytes = serde_json::to_vec(&record).expect("a synced record serializes");
            storage
                .set_data(&keys::portable_synced(&id.to_string()), Some(bytes))
                .await?;
        }
        Ok(renamed || changed.0 || changed.1 || changed.2)
    }

    /// Whether a state from the synced record is one to keep as a head:
    /// valid, `id`'s, signed by one of its roots, and adding no root
    /// through one removed here. Its revocations count either way, from
    /// the record's own list.
    async fn believes(
        &self,
        id: PortableUserId,
        heads: &[PortableIdentityState],
        state: &PortableIdentityState,
    ) -> Result<bool, SdkError> {
        if state.user != id || state.verify().is_err() {
            return Ok(false);
        }
        let genesis = super::key_roots::load_genesis(self.storage.as_ref(), id).await?;
        let before = spoolframe_protocol::portable::root_set(&id, genesis.as_ref(), heads);
        let mut all = heads.to_vec();
        all.push(state.clone());
        let after = spoolframe_protocol::portable::root_set(&id, genesis.as_ref(), &all);
        Ok(after.knew(&state.signer()) && !before.refuses_additions(&state.roots_added))
    }
}

/// The synced root of `id`, checked to be `id`'s; none synced is `None`.
async fn load_synced_root(
    storage: &dyn Storage,
    id: PortableUserId,
) -> Result<Option<PortableRoot>, StorageError> {
    let Some(bytes) = storage
        .secret(&keys::portable_root_synced(&id.to_string()))
        .await?
    else {
        return Ok(None);
    };
    let bytes = zeroize::Zeroizing::new(bytes);
    let secret: &[u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        StorageError(format!(
            "the synced root of portable identity {id} is {} bytes, not 32",
            bytes.len()
        ))
    })?;
    let root = PortableRoot::from_secret(secret);
    if root.id() != id {
        return Err(StorageError(format!(
            "the synced root for portable identity {id} is another identity's"
        )));
    }
    Ok(Some(root))
}

/// The synced record of `id`, if any; unreadable is an error, never
/// "none", so a damaged record is never overwritten with less.
async fn load_synced(
    storage: &dyn Storage,
    id: PortableUserId,
) -> Result<Option<SyncRecord>, StorageError> {
    match storage
        .data(&keys::portable_synced(&id.to_string()))
        .await?
    {
        None => Ok(None),
        Some(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| StorageError(format!("the synced record of {id} cannot be read: {e}"))),
    }
}

/// The revoked keys of `id` in its synced record, if sync is on and there
/// is one: what [`super::is_revoked`] reads besides this device's.
pub(super) async fn synced_revoked(
    storage: &dyn Storage,
    id: PortableUserId,
) -> Result<Revoked, StorageError> {
    Ok(load_synced(storage, id)
        .await?
        .map(|record| {
            let mut revoked = record.revoked;
            for head in &record.heads {
                super::merge(&mut revoked, &head.revoked);
            }
            revoked
        })
        .unwrap_or_default())
}

/// Add `from` to `into`, each key at its earliest. Whether it changed.
fn merge_revoked(into: &mut Revoked, from: &Revoked) -> bool {
    let entries: Vec<super::RevokedKey> = from
        .iter()
        .map(|(&key, &revoked_at)| super::RevokedKey { key, revoked_at })
        .collect();
    super::merge(into, &entries)
}

/// Add `from` to `into`, one entry per device key: the newer credential
/// (by issue time, then serial), with its label, or the other's if it has
/// none. Whether it changed.
fn merge_issued(into: &mut Vec<IssuedCredential>, from: &[IssuedCredential]) -> bool {
    let mut changed = false;
    for theirs in from {
        match into.iter_mut().find(|e| e.device_key == theirs.device_key) {
            None => {
                into.push(theirs.clone());
                changed = true;
            }
            Some(ours) => {
                let newer = (theirs.issued_at, theirs.serial) > (ours.issued_at, ours.serial);
                if newer {
                    let label = theirs.label.clone().or_else(|| ours.label.take());
                    *ours = IssuedCredential {
                        label,
                        ..theirs.clone()
                    };
                    changed = true;
                } else if ours.label.is_none() && theirs.label.is_some() {
                    ours.label = theirs.label.clone();
                    changed = true;
                }
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::portable::{
        DevicePublicKey, Portable, REVOCATION_WINDOW, RootKey, RootSigner, SoftSecurityKey,
        public_key,
    };
    use crate::storage::{MemoryStorage, Storage};

    /// A device: its storage (its keychain, as this device sees it) and
    /// its SDK.
    fn device() -> (Arc<MemoryStorage>, Portable) {
        let storage = Arc::new(MemoryStorage::default());
        (storage.clone(), Portable::new(storage))
    }

    fn key(n: u8) -> DevicePublicKey {
        public_key(&SigningKey::from_bytes(&[n; 32]))
    }

    /// iCloud Keychain carrying `from`'s synced items of `id` to `to`,
    /// the last writer's whole: each item replaces `to`'s, a removed one
    /// removes it.
    async fn deliver(from: &MemoryStorage, to: &MemoryStorage, id: PortableUserId) {
        let root = keys::portable_root_synced(&id.to_string());
        to.set_secret(&root, from.secret(&root).await.unwrap())
            .await
            .unwrap();
        let record = keys::portable_synced(&id.to_string());
        to.set_data(&record, from.data(&record).await.unwrap())
            .await
            .unwrap();
    }

    /// A phone with a phrase identity that added security key `k`.
    async fn phone_with_key() -> (
        Arc<MemoryStorage>,
        Portable,
        PortableUserId,
        SoftSecurityKey,
    ) {
        let (storage, phone) = device();
        let (id, _phrase) = phone.create("Sam").await.unwrap();
        let k = SoftSecurityKey::new();
        phone
            .add_root(id, RootSigner::Phrase, &k, "K")
            .await
            .unwrap();
        (storage, phone, id, k)
    }

    #[test]
    fn only_synced_keys_are_synced() {
        assert!(keys::is_synced(&keys::portable_root_synced("x")));
        assert!(keys::is_synced(&keys::portable_synced("x")));
        for local in [
            keys::portable_root("x"),
            keys::portable_device("x"),
            keys::portable_revoked("x"),
            keys::portable_issued("x"),
            keys::portable_state("x"),
            keys::portable_serial("x"),
            keys::PORTABLE_CREDENTIALS.to_owned(),
            keys::ENDPOINT_KEY.to_owned(),
        ] {
            assert!(!keys::is_synced(&local), "{local}");
        }
    }

    /// The user's decision: sync needs a security key root that is still
    /// one, so a lost synced device is shut out by removing the phrase
    /// root, not by starting over.
    #[tokio::test]
    async fn sync_needs_a_security_key_root_that_is_still_one() {
        let (_, phone) = device();
        let (id, _) = phone.create("Sam").await.unwrap();
        assert_eq!(
            phone.enable_root_sync(id).await.unwrap_err(),
            SdkError::RootSync(RootSyncRefused::NoSecurityKey)
        );
        let k = SoftSecurityKey::new();
        phone
            .add_root(id, RootSigner::Phrase, &k, "K")
            .await
            .unwrap();
        let k_root = k.credentials()[0].root();
        phone
            .remove_root(id, RootSigner::Phrase, &k_root)
            .await
            .unwrap();
        assert_eq!(
            phone.enable_root_sync(id).await.unwrap_err(),
            SdkError::RootSync(RootSyncRefused::NoSecurityKey)
        );
        assert_eq!(
            phone.root_location(id).await.unwrap(),
            RootLocation::ThisDevice
        );
    }

    #[tokio::test]
    async fn sync_is_refused_for_a_key_identity_and_a_removed_phrase_root() {
        let (_, desktop) = device();
        let (a, b) = (SoftSecurityKey::new(), SoftSecurityKey::new());
        let id = desktop.create_with_keys(&[&a, &b], "Sam").await.unwrap();
        assert_eq!(
            desktop.enable_root_sync(id).await.unwrap_err(),
            SdkError::RootSync(RootSyncRefused::KeyRooted)
        );

        let (_, phone, id, k) = phone_with_key().await;
        phone
            .remove_root(id, RootSigner::Key(&k), &RootKey::phrase_of(&id))
            .await
            .unwrap();
        assert_eq!(
            phone.enable_root_sync(id).await.unwrap_err(),
            SdkError::NotARoot
        );
        assert_eq!(
            phone.root_location(id).await.unwrap(),
            RootLocation::ThisDevice
        );
    }

    /// On, the root moves to its synced key and signs from there.
    #[tokio::test]
    async fn sync_moves_the_root_to_its_synced_key() {
        let (storage, phone, id, _k) = phone_with_key().await;
        phone.enable_root_sync(id).await.unwrap();
        assert_eq!(phone.root_location(id).await.unwrap(), RootLocation::Synced);
        assert!(phone.has_root(id).await.unwrap());
        assert_eq!(
            storage
                .secret(&keys::portable_root(&id.to_string()))
                .await
                .unwrap(),
            None
        );
        assert!(phone.list().await.unwrap()[0].has_root);
        phone
            .issue_credential(id, key(1), Some("TV"))
            .await
            .unwrap();
    }

    /// The un-revoke the user ruled out: a key revoked longer ago than any
    /// state or server remembers is refused by a holder that got the root
    /// through sync, from the synced revoked keys.
    #[tokio::test]
    async fn a_new_holder_never_issues_to_a_key_revoked_long_ago() {
        let (a_storage, phone, _, _) = phone_with_key().await;
        let id = phone.list().await.unwrap()[0].id;
        let long_ago = crate::portable::unix_now() - REVOCATION_WINDOW - 3600;
        phone.revoke_at(id, key(1), long_ago).await.unwrap();
        // A later state leaves the lapsed revocation out.
        let latest = phone.revoke(id, key(2)).await.unwrap();
        assert!(!latest.revokes(&key(1)));
        phone.enable_root_sync(id).await.unwrap();

        let (b_storage, ipad) = device();
        deliver(&a_storage, &b_storage, id).await;
        assert_eq!(ipad.adopt_synced(id, "Sam").await.unwrap(), id);
        assert_eq!(ipad.root_location(id).await.unwrap(), RootLocation::Synced);
        assert_eq!(
            ipad.issue_credential(id, key(1), None).await.unwrap_err(),
            SdkError::DeviceKeyRevoked
        );
        // It has the newest state too: its next is one past it.
        let next = ipad.revoke(id, key(3)).await.unwrap();
        assert_eq!(next.revision, latest.revision + 1);
        assert!(next.revokes(&key(2)));
    }

    /// Two synced holders each revoke and issue before either hears from
    /// the other, and the keychain keeps only the last write: each merge
    /// writes the union back, so both end with every revocation and every
    /// device, never one holder's alone.
    #[tokio::test]
    async fn revocations_and_devices_merge_by_union_not_last_writer() {
        let (a_storage, phone, id, _k) = phone_with_key().await;
        phone.enable_root_sync(id).await.unwrap();
        let (b_storage, ipad) = device();
        deliver(&a_storage, &b_storage, id).await;
        ipad.adopt_synced(id, "Sam").await.unwrap();

        phone
            .issue_credential(id, key(10), Some("TV"))
            .await
            .unwrap();
        phone.revoke(id, key(1)).await.unwrap();
        ipad.issue_credential(id, key(11), Some("Laptop"))
            .await
            .unwrap();
        ipad.revoke(id, key(2)).await.unwrap();
        // The iPad wrote last: its record replaces the phone's.
        deliver(&b_storage, &a_storage, id).await;
        phone.merge_synced(id).await.unwrap();
        deliver(&a_storage, &b_storage, id).await;
        ipad.merge_synced(id).await.unwrap();

        for holder in [&phone, &ipad] {
            for revoked in [key(1), key(2)] {
                assert_eq!(
                    holder
                        .issue_credential(id, revoked, None)
                        .await
                        .unwrap_err(),
                    SdkError::DeviceKeyRevoked
                );
            }
            let issued: Vec<_> = holder
                .issued(id)
                .await
                .unwrap()
                .into_iter()
                .map(|e| (e.device_key, e.label))
                .collect();
            assert!(issued.contains(&(key(10), Some("TV".into()))), "{issued:?}");
            assert!(
                issued.contains(&(key(11), Some("Laptop".into()))),
                "{issued:?}"
            );
            let heads = holder.identity_states(id).await.unwrap();
            assert_eq!(heads.len(), 1, "merged: {heads:?}");
            assert!(heads[0].revokes(&key(1)) && heads[0].revokes(&key(2)));
        }
    }

    /// Forgetting a synced identity removes this device's data only: the
    /// synced root stays for the user's other devices, and comes back.
    #[tokio::test]
    async fn forgetting_a_synced_identity_leaves_the_synced_root() {
        let (storage, phone, id, _k) = phone_with_key().await;
        phone.enable_root_sync(id).await.unwrap();
        phone.forget(id).await.unwrap();
        assert_eq!(phone.list().await.unwrap(), []);
        assert!(
            storage
                .secret(&keys::portable_root_synced(&id.to_string()))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            storage
                .data(&keys::portable_synced(&id.to_string()))
                .await
                .unwrap()
                .is_some()
        );
        phone.adopt_synced(id, "Sam").await.unwrap();
        phone.issue_credential(id, key(1), None).await.unwrap();
    }

    /// Off: each device first keeps a copy of its own, then one turns it
    /// off; the synced items go, every device keeps the root.
    #[tokio::test]
    async fn turning_sync_off_leaves_each_device_its_own_copy() {
        let (a_storage, phone, id, _k) = phone_with_key().await;
        phone.enable_root_sync(id).await.unwrap();
        let (b_storage, ipad) = device();
        deliver(&a_storage, &b_storage, id).await;
        ipad.adopt_synced(id, "Sam").await.unwrap();

        ipad.keep_root_on_device(id).await.unwrap();
        assert_eq!(
            ipad.root_location(id).await.unwrap(),
            RootLocation::ThisDeviceAndSynced
        );
        phone.disable_root_sync(id).await.unwrap();
        assert_eq!(
            phone.root_location(id).await.unwrap(),
            RootLocation::ThisDevice
        );
        deliver(&a_storage, &b_storage, id).await;
        assert_eq!(
            ipad.root_location(id).await.unwrap(),
            RootLocation::ThisDevice
        );
        assert_eq!(
            b_storage
                .data(&keys::portable_synced(&id.to_string()))
                .await
                .unwrap(),
            None
        );
        phone.issue_credential(id, key(1), None).await.unwrap();
        ipad.issue_credential(id, key(2), None).await.unwrap();
    }

    /// A phone and an iPad holding one synced identity, both listing it.
    async fn two_synced_devices() -> (
        (Arc<MemoryStorage>, Portable),
        (Arc<MemoryStorage>, Portable),
        PortableUserId,
    ) {
        let (a_storage, phone, id, _k) = phone_with_key().await;
        phone.enable_root_sync(id).await.unwrap();
        let (b_storage, ipad) = device();
        deliver(&a_storage, &b_storage, id).await;
        ipad.adopt_synced(id, "Sam").await.unwrap();
        ((a_storage, phone), (b_storage, ipad), id)
    }

    async fn name_of(holder: &Portable, id: PortableUserId) -> String {
        let listed = holder.list().await.unwrap();
        listed.into_iter().find(|e| e.id == id).unwrap().name
    }

    #[tokio::test]
    async fn a_rename_reaches_the_other_synced_device() {
        let ((a_storage, phone), (b_storage, ipad), id) = two_synced_devices().await;
        phone.rename(id, "Samantha").await.unwrap();
        deliver(&a_storage, &b_storage, id).await;
        assert!(ipad.merge_synced(id).await.unwrap());
        assert_eq!(name_of(&ipad, id).await, "Samantha");
        assert_eq!(name_of(&phone, id).await, "Samantha");
    }

    /// The iPad renames after it heard of the phone's rename: its rename
    /// is the newer, and both end with it.
    #[tokio::test]
    async fn a_later_rename_on_the_other_device_wins() {
        let ((a_storage, phone), (b_storage, ipad), id) = two_synced_devices().await;
        phone.rename(id, "Samantha").await.unwrap();
        deliver(&a_storage, &b_storage, id).await;
        ipad.merge_synced(id).await.unwrap();
        ipad.rename(id, "Sammy").await.unwrap();
        deliver(&b_storage, &a_storage, id).await;
        phone.merge_synced(id).await.unwrap();
        assert_eq!(name_of(&phone, id).await, "Sammy");
        assert_eq!(name_of(&ipad, id).await, "Sammy");
    }

    /// Both rename before either hears from the other, and the keychain
    /// keeps the older rename's record: the newer rename still wins on
    /// both, as the device that made it writes it back.
    #[tokio::test]
    async fn the_newer_of_two_unheard_renames_wins_whoever_wrote_last() {
        let ((a_storage, phone), (b_storage, ipad), id) = two_synced_devices().await;
        let now = crate::servers::now();
        phone.rename_at(id, "Samantha", now - 10).await.unwrap();
        ipad.rename_at(id, "Sammy", now).await.unwrap();
        // The phone wrote last: its record replaces the iPad's.
        deliver(&a_storage, &b_storage, id).await;
        ipad.merge_synced(id).await.unwrap();
        deliver(&b_storage, &a_storage, id).await;
        assert!(phone.merge_synced(id).await.unwrap());
        assert_eq!(name_of(&phone, id).await, "Sammy");
        assert_eq!(name_of(&ipad, id).await, "Sammy");
    }

    /// A record synced before names had a time still merges, its name
    /// taken as the oldest.
    #[tokio::test]
    async fn a_record_synced_without_a_name_time_merges() {
        let ((a_storage, phone), (b_storage, ipad), id) = two_synced_devices().await;
        let old = br#"{"name":"Old","created_at":1700000000}"#.to_vec();
        b_storage
            .set_data(&keys::portable_synced(&id.to_string()), Some(old))
            .await
            .unwrap();
        ipad.merge_synced(id).await.unwrap();
        // A rename beats the record's untimed name.
        ipad.rename(id, "New").await.unwrap();
        deliver(&b_storage, &a_storage, id).await;
        phone.merge_synced(id).await.unwrap();
        assert_eq!(name_of(&phone, id).await, "New");
    }

    #[tokio::test]
    async fn adopting_needs_the_identity_s_own_synced_root() {
        let (_, phone) = device();
        let (id, _) = phone.create("Sam").await.unwrap();
        let (b_storage, ipad) = device();
        assert_eq!(
            ipad.adopt_synced(id, "Sam").await.unwrap_err(),
            SdkError::UnknownIdentity(id.to_string())
        );
        b_storage
            .set_secret(
                &keys::portable_root_synced(&id.to_string()),
                Some(vec![7; 32]),
            )
            .await
            .unwrap();
        assert!(matches!(
            ipad.adopt_synced(id, "Sam").await.unwrap_err(),
            SdkError::Storage(_)
        ));
        assert_eq!(ipad.list().await.unwrap(), []);
    }
}
