//! Portable identities (P7-01, ARCHITECTURE §42–§46): a root signing key
//! the user owns, its [`PortableUserId`], and the recovery phrase that
//! gives the same key back on any device.
//!
//! The phone app holds the root (§44). It is signing-only: it authorizes
//! device credentials (P7-02) and is never itself sent anywhere.
//!
//! Device credentials (P7-02, §45, §48): each device that acts as an
//! identity holds its own key for it ([`Portable::device_key`]); the
//! root signs a [`UserDeviceCredential`] for that key
//! ([`Portable::issue_credential`]) and the device installs it
//! ([`Portable::install_credential`]), without needing the root itself.
//! The root revokes a key with a new [`PortableIdentityState`]
//! ([`Portable::revoke`]), which devices carry to servers. Nothing
//! here puts it, or the phrase, on the wire, and the server takes no
//! request that carries either.
//!
//! The phrase is BIP-39, English, 24 words (256 bits). The key comes from
//! it in two fixed steps: the standard BIP-39 seed (PBKDF2-HMAC-SHA512,
//! 2048 rounds, empty passphrase), then BLAKE3's key derivation under
//! [`ROOT_CONTEXT`] for the Ed25519 secret. The same phrase always gives
//! the same id; changing either step changes every id, and needs a new
//! context string.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use bip39::{Language, Mnemonic};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub use spoolframe_protocol::portable::{
    CredentialError, DevicePublicKey, MAX_CREDENTIAL_VALIDITY, PortableIdentityState,
    PortableUserId, REVOCATION_WINDOW, RevokedKey, Signature, StateError, TieProof,
    UserDeviceCredential, revocation_lapsed,
};
use spoolframe_protocol::portable::{device_proof_message, tie_message};

use crate::error::SdkError;
use crate::servers::now;
use crate::storage::{Storage, StorageError, keys};

/// Words in a recovery phrase: 24, for 256 bits of entropy.
pub const PHRASE_WORDS: usize = 24;

/// The BLAKE3 context the root key is derived under. Fixed forever: a new
/// derivation takes a new string.
pub const ROOT_CONTEXT: &str = "spoolframe 2026-10-09 portable root v1";

/// Bytes of entropy behind a phrase.
const ENTROPY_LEN: usize = 32;

/// What is wrong with a recovery phrase as typed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PhraseError {
    /// Not 24 words.
    #[error("a recovery phrase is {PHRASE_WORDS} words; this is {found}")]
    WordCount { found: usize },
    /// Word `position` (from 1) is not in the BIP-39 English wordlist:
    /// misspelt, most likely. The word itself is not kept, so the error
    /// can be logged.
    #[error("word {position} is not a recovery phrase word")]
    UnknownWord { position: usize },
    /// Every word is a real one, but they do not check out: one is wrong
    /// or out of place.
    #[error("the words do not check out; one is wrong or out of place")]
    Checksum,
}

/// The phrase a new identity is shown with once, to write down. Its words
/// are cleared from memory when it is dropped. [`fmt::Display`] gives
/// them, space-separated; [`fmt::Debug`] does not.
pub struct RecoveryPhrase(Mnemonic);

impl RecoveryPhrase {
    /// The words, in order, for an app that lays them out one by one.
    pub fn words(&self) -> Vec<String> {
        self.0.words().map(str::to_owned).collect()
    }
}

impl fmt::Display for RecoveryPhrase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for RecoveryPhrase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecoveryPhrase(<redacted>)")
    }
}

/// A portable identity's root: the signing key behind a
/// [`PortableUserId`]. Signing-only, and never sent: what leaves the
/// device is what it signs. Cleared from memory when dropped; no
/// serialization, and [`fmt::Debug`] shows only the id.
pub struct PortableRoot {
    key: SigningKey,
    id: PortableUserId,
}

impl PortableRoot {
    /// A new root from the OS's randomness, and the phrase that recovers
    /// it. Show the phrase once; it is not stored.
    pub fn generate() -> (PortableRoot, RecoveryPhrase) {
        let mut entropy = Zeroizing::new([0u8; ENTROPY_LEN]);
        getrandom::fill(entropy.as_mut()).expect("the OS gives random bytes");
        let mnemonic =
            Mnemonic::from_entropy_in(Language::English, entropy.as_ref()).expect("32 bytes");
        let root = Self::from_mnemonic(&mnemonic);
        (root, RecoveryPhrase(mnemonic))
    }

    /// The root `phrase` gives: the same one, every time, on any device.
    /// Spacing and case do not matter.
    pub fn recover(phrase: &str) -> Result<PortableRoot, PhraseError> {
        Ok(Self::from_mnemonic(&parse(phrase)?))
    }

    pub fn id(&self) -> PortableUserId {
        self.id
    }

    /// The root's signature over `message`, which
    /// [`PortableUserId::verify`] checks. Sign only what this SDK builds
    /// (device credentials, identity state); the root itself is never
    /// sent anywhere.
    pub fn sign(&self, message: &[u8]) -> Signature {
        self.key.sign(message)
    }

    fn from_mnemonic(mnemonic: &Mnemonic) -> Self {
        let seed = Zeroizing::new(mnemonic.to_seed_normalized(""));
        let secret = Zeroizing::new(blake3::derive_key(ROOT_CONTEXT, seed.as_ref()));
        Self::from_secret(&secret)
    }

    /// The root whose 32-byte secret is `secret`, as stored.
    fn from_secret(secret: &[u8; 32]) -> Self {
        let key = SigningKey::from_bytes(secret);
        let id = PortableUserId::from_bytes(key.verifying_key().to_bytes())
            .expect("an Ed25519 public key is a point on the curve");
        Self { key, id }
    }

    fn secret(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.key.to_bytes())
    }
}

impl fmt::Debug for PortableRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortableRoot")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// The phrase as typed, made canonical: trimmed, single-spaced, lower
/// case. Then BIP-39's own checks: its count, its words, its checksum.
fn parse(phrase: &str) -> Result<Mnemonic, PhraseError> {
    let normalized = Zeroizing::new(
        phrase
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
            .join(" "),
    );
    let found = normalized.split(' ').filter(|w| !w.is_empty()).count();
    if found != PHRASE_WORDS {
        return Err(PhraseError::WordCount { found });
    }
    Mnemonic::parse_in_normalized(Language::English, &normalized).map_err(|e| match e {
        bip39::Error::UnknownWord(index) => PhraseError::UnknownWord {
            position: index + 1,
        },
        bip39::Error::BadWordCount(found) => PhraseError::WordCount { found },
        _ => PhraseError::Checksum,
    })
}

/// A portable identity this device holds the root of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableIdentity {
    pub id: PortableUserId,
    /// What the user called it on this device; theirs alone, never sent.
    pub name: String,
    /// Unix seconds: made, or first recovered, here.
    pub created_at: u64,
    /// Whether its root is here now, as [`Portable::list`] found it. Not
    /// stored: an app's storage restored from a backup that leaves out
    /// secrets (the iOS keychain's this-device-only items) lists the
    /// identity without its root. Without it nothing is issued, revoked
    /// or tied with the root; recovering from the phrase puts it back.
    #[serde(skip)]
    pub has_root: bool,
}

/// A device the root gave a credential to, as kept on the root's phone:
/// the newest credential for that device key. Nothing here is sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedCredential {
    pub device_key: DevicePublicKey,
    pub serial: u64,
    /// Unix seconds.
    pub issued_at: i64,
    /// Unix seconds.
    pub expires_at: i64,
    /// What the user called the device when issuing ("Living room TV");
    /// theirs alone. A renewal without one keeps the one before.
    pub label: Option<String>,
    /// Unix seconds: revoked here (by this root, or in a state accepted
    /// from elsewhere), however long ago. Not stored: read from the
    /// revoked keys when listed.
    #[serde(skip)]
    pub revoked_at: Option<i64>,
}

/// The portable identities on this device, kept in the app's [`Storage`]:
/// each root as a secret ([`keys::portable_root`]), the list as data
/// ([`keys::PORTABLE_IDENTITIES`]). Cheap to clone; clones share one lock,
/// so take them from [`crate::Client::portable`] or clone one value
/// rather than making several over the same storage.
#[derive(Clone)]
pub struct Portable {
    storage: Arc<dyn Storage>,
    /// Serializes read-modify-write of the list.
    saving: Arc<tokio::sync::Mutex<()>>,
}

impl fmt::Debug for Portable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Portable").finish_non_exhaustive()
    }
}

impl Portable {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self {
            storage,
            saving: Arc::default(),
        }
    }

    /// A new identity called `name`, kept on this device. Show the phrase
    /// once and ask the user to write it down: it is the only way back to
    /// the identity, and the SDK does not keep it.
    pub async fn create(&self, name: &str) -> Result<(PortableUserId, RecoveryPhrase), SdkError> {
        let (root, phrase) = PortableRoot::generate();
        self.keep(&root, name).await?;
        tracing::info!(id = %root.id(), "made a portable identity");
        Ok((root.id(), phrase))
    }

    /// The identity `phrase` recovers, kept on this device as `name`.
    /// Recovering one already here changes nothing; it keeps its name.
    pub async fn recover(&self, phrase: &str, name: &str) -> Result<PortableUserId, SdkError> {
        let root = PortableRoot::recover(phrase)?;
        self.keep(&root, name).await?;
        tracing::info!(id = %root.id(), "recovered a portable identity");
        Ok(root.id())
    }

    /// The identities on this device, oldest first, each with whether its
    /// root is here ([`PortableIdentity::has_root`]).
    pub async fn list(&self) -> Result<Vec<PortableIdentity>, SdkError> {
        let storage = self.storage.as_ref();
        let mut listed = load(storage).await?;
        for entry in &mut listed {
            entry.has_root = storage
                .secret(&keys::portable_root(&entry.id.to_string()))
                .await?
                .is_some();
        }
        Ok(listed)
    }

    /// The devices identity `id`'s root has given credentials to here,
    /// in the order first issued: the newest credential of each, revoked
    /// ones too ([`IssuedCredential::revoked_at`]). Empty on a device
    /// that never issued for it.
    pub async fn issued(&self, id: PortableUserId) -> Result<Vec<IssuedCredential>, SdkError> {
        let storage = self.storage.as_ref();
        let mut issued = load_issued(storage, id).await?;
        let revoked = load_revoked(storage, id).await?;
        let state = load_state(storage, id).await?;
        for entry in &mut issued {
            entry.revoked_at = revoked.get(&entry.device_key).copied().or_else(|| {
                state
                    .iter()
                    .flat_map(|s| &s.revoked)
                    .find(|r| r.key == entry.device_key)
                    .map(|r| r.revoked_at)
            });
        }
        Ok(issued)
    }

    /// Remove identity `id` from this device: its root and its entry. Its
    /// phrase still recovers it. Forgetting one not here does nothing.
    pub async fn forget(&self, id: PortableUserId) -> Result<(), SdkError> {
        let storage = self.storage.as_ref();
        let _saving = self.saving.lock().await;
        // The secret first: should the list then fail to save, an entry
        // with no root stays, which forgetting again clears, rather than
        // a root nothing lists.
        storage
            .set_secret(&keys::portable_root(&id.to_string()), None)
            .await?;
        let mut saved = load(storage).await?;
        saved.retain(|entry| entry.id != id);
        save(storage, &saved).await?;
        Ok(())
    }

    /// Identity `id`'s root, for signing credentials and states.
    pub(crate) async fn load_root(&self, id: PortableUserId) -> Result<PortableRoot, SdkError> {
        let Some(bytes) = self
            .storage
            .secret(&keys::portable_root(&id.to_string()))
            .await?
        else {
            return Err(SdkError::UnknownIdentity(id.to_string()));
        };
        let bytes = Zeroizing::new(bytes);
        let secret: &[u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            StorageError(format!(
                "the root of portable identity {id} is {} bytes, not 32",
                bytes.len()
            ))
        })?;
        let root = PortableRoot::from_secret(secret);
        if root.id() != id {
            return Err(StorageError(format!(
                "the root stored for portable identity {id} is another identity's"
            ))
            .into());
        }
        Ok(root)
    }

    // Root side (P7-02): the phone that holds identity `id`'s root.

    /// A credential from identity `id`'s root for `device_key`, good for
    /// [`MAX_CREDENTIAL_VALIDITY`] from now. Renewal is the same call: a
    /// new credential for the same key, with the next serial. The serial
    /// is saved before the credential is signed, so a failure in between
    /// skips one rather than giving two credentials the same serial.
    /// Refused for a key revoked here, however long ago.
    ///
    /// Recorded for [`Portable::issued`] under `label`, what the user
    /// calls the device; `None` keeps the label it had. Should the record
    /// fail to save, the credential is not given out: the list of devices
    /// stays whole.
    pub async fn issue_credential(
        &self,
        id: PortableUserId,
        device_key: DevicePublicKey,
        label: Option<&str>,
    ) -> Result<UserDeviceCredential, SdkError> {
        self.issue_credential_at(id, device_key, label, unix_now())
            .await
    }

    async fn issue_credential_at(
        &self,
        id: PortableUserId,
        device_key: DevicePublicKey,
        label: Option<&str>,
        now: i64,
    ) -> Result<UserDeviceCredential, SdkError> {
        let storage = self.storage.as_ref();
        let root = self.load_root(id).await?;
        let _saving = self.saving.lock().await;
        if is_revoked(storage, id, &device_key).await? {
            return Err(SdkError::DeviceKeyRevoked);
        }
        let serial_key = keys::portable_serial(&id.to_string());
        let serial = match storage.data(&serial_key).await? {
            None => 0,
            Some(bytes) => serde_json::from_slice::<u64>(&bytes).map_err(|e| {
                StorageError(format!("the credential serial of {id} cannot be read: {e}"))
            })?,
        }
        .checked_add(1)
        .ok_or_else(|| StorageError(format!("the credential serial of {id} is at its end")))?;
        storage
            .set_data(&serial_key, Some(serial.to_string().into_bytes()))
            .await?;
        let credential = UserDeviceCredential::new(
            id,
            device_key,
            serial,
            now,
            now.saturating_add(MAX_CREDENTIAL_VALIDITY),
            |message| root.sign(message),
        );
        let mut issued = load_issued(storage, id).await?;
        let record = IssuedCredential {
            device_key,
            serial,
            issued_at: credential.issued_at,
            expires_at: credential.expires_at,
            label: None,
            revoked_at: None,
        };
        match issued.iter_mut().find(|e| e.device_key == device_key) {
            Some(entry) => {
                let label = label.map(str::to_owned).or(entry.label.take());
                *entry = IssuedCredential { label, ..record };
            }
            None => issued.push(IssuedCredential {
                label: label.map(str::to_owned),
                ..record
            }),
        }
        save_issued(storage, id, &issued).await?;
        tracing::info!(%id, %device_key, serial, "issued a portable device credential");
        Ok(credential)
    }

    /// Revoke `device_key` for identity `id`, from now: a new state from
    /// its root, one revision past the newest here, listing every key
    /// revoked here (by this root, or in a state accepted from elsewhere)
    /// that has not lapsed ([`revocation_lapsed`]), and `device_key`.
    /// Kept here as the newest; carry it to servers. A key the newest
    /// state already revokes gives it back unchanged. More than the
    /// protocol's limit of live revocations is refused
    /// ([`StateError::TooManyRevoked`]).
    pub async fn revoke(
        &self,
        id: PortableUserId,
        device_key: DevicePublicKey,
    ) -> Result<PortableIdentityState, SdkError> {
        self.revoke_at(id, device_key, unix_now()).await
    }

    async fn revoke_at(
        &self,
        id: PortableUserId,
        device_key: DevicePublicKey,
        now: i64,
    ) -> Result<PortableIdentityState, SdkError> {
        let storage = self.storage.as_ref();
        let root = self.load_root(id).await?;
        let _saving = self.saving.lock().await;
        let previous = load_state(storage, id).await?;
        if let Some(previous) = previous.as_ref().filter(|s| s.revokes(&device_key)) {
            return Ok(previous.clone());
        }
        let revision = match &previous {
            None => 1,
            Some(previous) => previous.revision.checked_add(1).ok_or_else(|| {
                StorageError(format!(
                    "the identity state of {id} is at its last revision"
                ))
            })?,
        };
        let mut union = load_revoked(storage, id).await?;
        if let Some(previous) = &previous {
            merge(&mut union, &previous.revoked);
        }
        // Revoked from now, unless it already is (from a state accepted
        // from elsewhere) and that revocation has not lapsed.
        let revoked_at = union
            .get(&device_key)
            .copied()
            .filter(|&at| !revocation_lapsed(at, now))
            .unwrap_or(now);
        union.insert(device_key, revoked_at);
        let live = union
            .iter()
            .filter(|&(_, &at)| !revocation_lapsed(at, now))
            .map(|(&key, &revoked_at)| RevokedKey { key, revoked_at })
            .collect();
        let state =
            PortableIdentityState::new(id, revision, live, now, |message| root.sign(message))?;
        // The union first: should the state then fail to save, the key
        // is still refused here, and revoking it again signs anew.
        save_revoked(storage, id, &union).await?;
        save_state(storage, &state).await?;
        tracing::info!(%id, %device_key, revision, "revoked a portable device key");
        Ok(state)
    }

    /// The newest identity state of `id` this device has: made here, or
    /// accepted from elsewhere. `None` if it has none: nothing revoked
    /// that it knows of.
    pub async fn identity_state(
        &self,
        id: PortableUserId,
    ) -> Result<Option<PortableIdentityState>, SdkError> {
        Ok(load_state(self.storage.as_ref(), id).await?)
    }

    // Device side (P7-02): any device that acts as an identity, the
    // root's own phone or a TV that never holds it.

    /// This device's key for acting as identity `id`, made and kept the
    /// first time it is asked for. Show it to the root's phone (P7-03's
    /// QR) to get a credential for it. A key revoked here is replaced by
    /// a new one: a revoked key never authenticates again, so the device
    /// pairs afresh with a key that can.
    pub async fn device_key(&self, id: PortableUserId) -> Result<DevicePublicKey, SdkError> {
        let _saving = self.saving.lock().await;
        let storage = self.storage.as_ref();
        if let Some(key) = load_device_key(storage, id).await? {
            let key = public_key(&key);
            if !is_revoked(storage, id, &key).await? {
                return Ok(key);
            }
            tracing::info!(%id, revoked = %key, "replacing a revoked portable device key");
        }
        let mut secret = Zeroizing::new([0u8; 32]);
        getrandom::fill(secret.as_mut()).expect("the OS gives random bytes");
        storage
            .set_secret(
                &keys::portable_device(&id.to_string()),
                Some(secret.to_vec()),
            )
            .await?;
        let key = public_key(&SigningKey::from_bytes(&secret));
        tracing::info!(%id, device_key = %key, "made a portable device key");
        Ok(key)
    }

    /// Keep `credential` as the one this device acts as its user with,
    /// replacing any before it. Refused unless it holds now
    /// ([`UserDeviceCredential::verify`]), names this device's key for
    /// that user ([`SdkError::NotThisDevice`]), and is not revoked here.
    pub async fn install_credential(
        &self,
        credential: UserDeviceCredential,
    ) -> Result<(), SdkError> {
        self.install_credential_at(credential, unix_now()).await
    }

    async fn install_credential_at(
        &self,
        credential: UserDeviceCredential,
        now: i64,
    ) -> Result<(), SdkError> {
        credential.verify(now)?;
        let id = credential.user;
        let _saving = self.saving.lock().await;
        let storage = self.storage.as_ref();
        let ours = load_device_key(storage, id).await?;
        if ours.is_none_or(|key| public_key(&key) != credential.device_key) {
            return Err(SdkError::NotThisDevice);
        }
        if is_revoked(storage, id, &credential.device_key).await? {
            return Err(SdkError::DeviceKeyRevoked);
        }
        let mut credentials = load_credentials(storage).await?;
        credentials.insert(id, credential);
        save_credentials(storage, &credentials).await?;
        tracing::info!(%id, "installed a portable device credential");
        Ok(())
    }

    /// The credential this device acts as identity `id` with, if any.
    pub async fn credential(
        &self,
        id: PortableUserId,
    ) -> Result<Option<UserDeviceCredential>, SdkError> {
        Ok(load_credentials(self.storage.as_ref()).await?.remove(&id))
    }

    /// Take in `state`, a state another device brought: its revocations
    /// join the ones here whatever its revision (revocation only grows),
    /// and it is kept as the newest, to carry on to servers, if its
    /// revision is higher than the newest here. Whether it was kept as
    /// the newest. Only for identities this device knows (holds the root
    /// of, or a credential for); a state for another is
    /// [`SdkError::UnknownIdentity`].
    pub async fn accept_state(&self, state: PortableIdentityState) -> Result<bool, SdkError> {
        state.verify()?;
        let id = state.user;
        let storage = self.storage.as_ref();
        let _saving = self.saving.lock().await;
        let known = load(storage).await?.iter().any(|entry| entry.id == id)
            || load_credentials(storage).await?.contains_key(&id);
        if !known {
            return Err(SdkError::UnknownIdentity(id.to_string()));
        }
        let mut union = load_revoked(storage, id).await?;
        if merge(&mut union, &state.revoked) {
            save_revoked(storage, id, &union).await?;
        }
        if load_state(storage, id)
            .await?
            .is_some_and(|newest| newest.revision >= state.revision)
        {
            return Ok(false);
        }
        save_state(storage, &state).await?;
        tracing::info!(%id, revision = state.revision, "accepted a newer portable identity state");
        Ok(true)
    }

    /// What this device sends to sign in as identity `id` on the server
    /// whose endpoint id is `server`, answering its `nonce`: the
    /// credential, and the device key's signature over
    /// [`device_proof_message`]. The request that carries them is P7-06.
    /// Refused if the credential no longer holds (renew it from the
    /// root) or its key is revoked here.
    pub async fn prove(
        &self,
        id: PortableUserId,
        server: &[u8; 32],
        nonce: &[u8; 32],
    ) -> Result<(UserDeviceCredential, Signature), SdkError> {
        self.prove_at(id, server, nonce, unix_now()).await
    }

    async fn prove_at(
        &self,
        id: PortableUserId,
        server: &[u8; 32],
        nonce: &[u8; 32],
        now: i64,
    ) -> Result<(UserDeviceCredential, Signature), SdkError> {
        let storage = self.storage.as_ref();
        let credential = load_credentials(storage)
            .await?
            .remove(&id)
            .ok_or_else(|| SdkError::UnknownIdentity(id.to_string()))?;
        credential.verify(now)?;
        if is_revoked(storage, id, &credential.device_key).await? {
            return Err(SdkError::DeviceKeyRevoked);
        }
        let key = load_device_key(storage, id)
            .await?
            .filter(|key| public_key(key) == credential.device_key)
            .ok_or(SdkError::NotThisDevice)?;
        let signature = key.sign(&device_proof_message(server, nonce));
        Ok((credential, signature))
    }

    /// What ties the local account `username` on the server whose endpoint
    /// id is `server` to identity `id`, answering its `nonce` (P7-04): the
    /// root's signature over [`tie_message`] if this device holds the
    /// root, else this device key's with its credential, checked as
    /// [`Portable::prove`] checks them. The root itself is never sent.
    /// `server` is the id this device dialled, never one a server named.
    pub async fn tie_proof(
        &self,
        id: PortableUserId,
        server: &[u8; 32],
        nonce: &[u8; 32],
        username: &str,
    ) -> Result<TieProof, SdkError> {
        self.tie_proof_at(id, server, nonce, username, unix_now())
            .await
    }

    async fn tie_proof_at(
        &self,
        id: PortableUserId,
        server: &[u8; 32],
        nonce: &[u8; 32],
        username: &str,
        now: i64,
    ) -> Result<TieProof, SdkError> {
        let message = tie_message(server, nonce, &id, username)
            .ok_or_else(|| SdkError::Protocol("the account's username is too long".into()))?;
        match self.load_root(id).await {
            Ok(root) => {
                return Ok(TieProof::Root {
                    signature: root.sign(&message),
                });
            }
            Err(SdkError::UnknownIdentity(_)) => {}
            Err(e) => return Err(e),
        }
        let storage = self.storage.as_ref();
        let credential = load_credentials(storage)
            .await?
            .remove(&id)
            .ok_or_else(|| SdkError::UnknownIdentity(id.to_string()))?;
        credential.verify(now)?;
        if is_revoked(storage, id, &credential.device_key).await? {
            return Err(SdkError::DeviceKeyRevoked);
        }
        let key = load_device_key(storage, id)
            .await?
            .filter(|key| public_key(key) == credential.device_key)
            .ok_or(SdkError::NotThisDevice)?;
        let signature = key.sign(&message);
        Ok(TieProof::Device {
            credential,
            signature,
        })
    }

    // Guests (P7-03): identities this device acts as without holding
    // their root, by the device's own leave.

    /// Whether this device lets people sign in on it as an account that
    /// is not its own: off unless the user turned it on. The shared
    /// living-room TV turns it on; a phone or a laptop keeps it off. Off,
    /// the device shows no pairing code and takes no pairing
    /// ([`crate::Client::start_pairing`]), before anything is sent
    /// anywhere.
    ///
    /// The one gate for every external principal: a portable identity
    /// paired by QR now (P7-03), a federated server's user when P8 pairs
    /// servers. Storage that cannot be read is an error, never "on".
    pub async fn allow_guests(&self) -> Result<bool, SdkError> {
        match self.storage.data(keys::ALLOW_GUESTS).await? {
            None => Ok(false),
            Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                StorageError(format!("the guest sign-in setting cannot be read: {e}")).into()
            }),
        }
    }

    /// Turn guest sign-ins on or off ([`Portable::allow_guests`]). Turning
    /// it off refuses any pairing from then on, one under way included;
    /// guests already here stay until forgotten
    /// ([`Portable::forget_guest`]).
    pub async fn set_allow_guests(&self, allowed: bool) -> Result<(), SdkError> {
        self.storage
            .set_data(
                keys::ALLOW_GUESTS,
                Some(serde_json::to_vec(&allowed).expect("a bool serializes")),
            )
            .await?;
        tracing::info!(allowed, "guest sign-ins changed");
        Ok(())
    }

    /// The identities this device holds a credential for but not the root
    /// of: the guests paired onto it, oldest id first. Each signs out on
    /// its own ([`Portable::forget_guest`]).
    pub async fn guests(&self) -> Result<Vec<PortableUserId>, SdkError> {
        let storage = self.storage.as_ref();
        let mut guests = Vec::new();
        for id in load_credentials(storage).await?.into_keys() {
            if storage
                .secret(&keys::portable_root(&id.to_string()))
                .await?
                .is_none()
            {
                guests.push(id);
            }
        }
        Ok(guests)
    }

    /// Sign guest `id` out of this device: its credential, its device key
    /// and the state it brought go. Other guests are untouched. Refused
    /// for an identity whose root is here ([`Portable::forget`] is for
    /// that). Forgetting one not here does nothing.
    pub async fn forget_guest(&self, id: PortableUserId) -> Result<(), SdkError> {
        let storage = self.storage.as_ref();
        if storage
            .secret(&keys::portable_root(&id.to_string()))
            .await?
            .is_some()
        {
            return Err(SdkError::InvalidInput(format!(
                "this device holds the root of {id}: it is not a guest"
            )));
        }
        let _saving = self.saving.lock().await;
        // The credential first: should the rest fail, nothing acts as
        // the guest, and forgetting again clears what is left.
        let mut credentials = load_credentials(storage).await?;
        if credentials.remove(&id).is_some() {
            save_credentials(storage, &credentials).await?;
        }
        storage
            .set_secret(&keys::portable_device(&id.to_string()), None)
            .await?;
        storage
            .set_data(&keys::portable_state(&id.to_string()), None)
            .await?;
        tracing::info!(%id, "forgot a guest");
        Ok(())
    }

    /// Whether this device has made a key for acting as `id`.
    pub(crate) async fn has_device_key(&self, id: PortableUserId) -> Result<bool, SdkError> {
        Ok(load_device_key(self.storage.as_ref(), id).await?.is_some())
    }

    /// Remove this device's key for `id` unless a credential names it: a
    /// pairing that made the key and did not finish leaves nothing.
    pub(crate) async fn drop_unused_device_key(&self, id: PortableUserId) -> Result<(), SdkError> {
        let _saving = self.saving.lock().await;
        let storage = self.storage.as_ref();
        if load_credentials(storage).await?.contains_key(&id) {
            return Ok(());
        }
        storage
            .set_secret(&keys::portable_device(&id.to_string()), None)
            .await?;
        Ok(())
    }

    /// Remove the credential this device acts as `id` with, if any.
    pub(crate) async fn drop_credential(&self, id: PortableUserId) -> Result<(), SdkError> {
        let _saving = self.saving.lock().await;
        let storage = self.storage.as_ref();
        let mut credentials = load_credentials(storage).await?;
        if credentials.remove(&id).is_some() {
            save_credentials(storage, &credentials).await?;
        }
        Ok(())
    }

    /// Store `root`'s secret, then list it if it is not listed yet. The
    /// secret goes first, so a listed identity always has its root.
    async fn keep(&self, root: &PortableRoot, name: &str) -> Result<(), SdkError> {
        let storage = self.storage.as_ref();
        let id = root.id();
        let _saving = self.saving.lock().await;
        let mut saved = load(storage).await?;
        storage
            .set_secret(
                &keys::portable_root(&id.to_string()),
                Some(root.secret().to_vec()),
            )
            .await?;
        if saved.iter().all(|entry| entry.id != id) {
            saved.push(PortableIdentity {
                id,
                name: name.to_owned(),
                created_at: now(),
                has_root: true,
            });
            save(storage, &saved).await?;
        }
        Ok(())
    }
}

/// The list; a list that cannot be read is an error, never "none".
async fn load(storage: &dyn Storage) -> Result<Vec<PortableIdentity>, StorageError> {
    match storage.data(keys::PORTABLE_IDENTITIES).await? {
        None => Ok(Vec::new()),
        Some(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| StorageError(format!("the portable identities cannot be read: {e}"))),
    }
}

async fn save(storage: &dyn Storage, saved: &[PortableIdentity]) -> Result<(), StorageError> {
    let bytes = serde_json::to_vec(saved).expect("portable identities always serialize");
    storage
        .set_data(keys::PORTABLE_IDENTITIES, Some(bytes))
        .await
}

/// Every device key revoked here for one identity, and the earliest time
/// it was revoked: by this root, or in any valid state accepted from
/// elsewhere, older revisions too. Never pruned: it is what stops this
/// root issuing to a key it once revoked, long after the signed states
/// have dropped it.
type Revoked = BTreeMap<DevicePublicKey, i64>;

/// Add `entries` to `union`, each at its earliest. Whether it changed.
fn merge(union: &mut Revoked, entries: &[RevokedKey]) -> bool {
    let mut changed = false;
    for entry in entries {
        use std::collections::btree_map::Entry;
        match union.entry(entry.key) {
            Entry::Vacant(vacant) => {
                vacant.insert(entry.revoked_at);
                changed = true;
            }
            Entry::Occupied(mut at) if entry.revoked_at < *at.get() => {
                at.insert(entry.revoked_at);
                changed = true;
            }
            Entry::Occupied(_) => {}
        }
    }
    changed
}

/// Whether `key` is revoked here for `id`: in the union, or in the
/// newest state.
async fn is_revoked(
    storage: &dyn Storage,
    id: PortableUserId,
    key: &DevicePublicKey,
) -> Result<bool, StorageError> {
    Ok(load_revoked(storage, id).await?.contains_key(key)
        || load_state(storage, id)
            .await?
            .is_some_and(|state| state.revokes(key)))
}

/// The union for `id`; unreadable is an error, never "none".
async fn load_revoked(storage: &dyn Storage, id: PortableUserId) -> Result<Revoked, StorageError> {
    match storage
        .data(&keys::portable_revoked(&id.to_string()))
        .await?
    {
        None => Ok(Revoked::new()),
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            StorageError(format!(
                "the revoked device keys of {id} cannot be read: {e}"
            ))
        }),
    }
}

async fn save_revoked(
    storage: &dyn Storage,
    id: PortableUserId,
    union: &Revoked,
) -> Result<(), StorageError> {
    let bytes = serde_json::to_vec(union).expect("revoked keys always serialize");
    storage
        .set_data(&keys::portable_revoked(&id.to_string()), Some(bytes))
        .await
}

/// The devices `id`'s root issued to; unreadable is an error, never
/// "none".
async fn load_issued(
    storage: &dyn Storage,
    id: PortableUserId,
) -> Result<Vec<IssuedCredential>, StorageError> {
    match storage
        .data(&keys::portable_issued(&id.to_string()))
        .await?
    {
        None => Ok(Vec::new()),
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            StorageError(format!(
                "the devices issued credentials by {id} cannot be read: {e}"
            ))
        }),
    }
}

async fn save_issued(
    storage: &dyn Storage,
    id: PortableUserId,
    issued: &[IssuedCredential],
) -> Result<(), StorageError> {
    let bytes = serde_json::to_vec(issued).expect("issued credentials always serialize");
    storage
        .set_data(&keys::portable_issued(&id.to_string()), Some(bytes))
        .await
}

/// Unix seconds now, as credentials count them.
fn unix_now() -> i64 {
    i64::try_from(now()).unwrap_or(i64::MAX)
}

fn public_key(key: &SigningKey) -> DevicePublicKey {
    DevicePublicKey::from_bytes(key.verifying_key().to_bytes())
        .expect("an Ed25519 public key is a point on the curve")
}

/// This device's key for identity `id`, if it has made one.
async fn load_device_key(
    storage: &dyn Storage,
    id: PortableUserId,
) -> Result<Option<SigningKey>, StorageError> {
    let Some(bytes) = storage
        .secret(&keys::portable_device(&id.to_string()))
        .await?
    else {
        return Ok(None);
    };
    let bytes = Zeroizing::new(bytes);
    let secret: &[u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        StorageError(format!(
            "the device key for portable identity {id} is {} bytes, not 32",
            bytes.len()
        ))
    })?;
    Ok(Some(SigningKey::from_bytes(secret)))
}

/// The installed credentials; unreadable is an error, never "none".
async fn load_credentials(
    storage: &dyn Storage,
) -> Result<BTreeMap<PortableUserId, UserDeviceCredential>, StorageError> {
    match storage.data(keys::PORTABLE_CREDENTIALS).await? {
        None => Ok(BTreeMap::new()),
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            StorageError(format!(
                "the portable device credentials cannot be read: {e}"
            ))
        }),
    }
}

async fn save_credentials(
    storage: &dyn Storage,
    credentials: &BTreeMap<PortableUserId, UserDeviceCredential>,
) -> Result<(), StorageError> {
    let bytes = serde_json::to_vec(credentials).expect("credentials always serialize");
    storage
        .set_data(keys::PORTABLE_CREDENTIALS, Some(bytes))
        .await
}

/// The newest state of `id` here; unreadable is an error, never "none".
async fn load_state(
    storage: &dyn Storage,
    id: PortableUserId,
) -> Result<Option<PortableIdentityState>, StorageError> {
    let Some(bytes) = storage.data(&keys::portable_state(&id.to_string())).await? else {
        return Ok(None);
    };
    let state: PortableIdentityState = serde_json::from_slice(&bytes)
        .map_err(|e| StorageError(format!("the identity state of {id} cannot be read: {e}")))?;
    if state.user != id {
        return Err(StorageError(format!(
            "the identity state stored for {id} is another identity's"
        )));
    }
    Ok(Some(state))
}

async fn save_state(
    storage: &dyn Storage,
    state: &PortableIdentityState,
) -> Result<(), StorageError> {
    let bytes = serde_json::to_vec(state).expect("an identity state always serializes");
    storage
        .set_data(&keys::portable_state(&state.user.to_string()), Some(bytes))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryStorage;
    use async_trait::async_trait;

    /// BIP-39's all-zero 256-bit entropy: 23 × "abandon", then "art".
    fn zero_phrase() -> String {
        let mut words = vec!["abandon"; 23];
        words.push("art");
        words.join(" ")
    }

    /// The id [`zero_phrase`] gives. Written out, so any change to the
    /// derivation (seed, context, curve) fails here before it strands a
    /// user's identity. Computed apart from this code: Python's
    /// `hashlib.pbkdf2_hmac` for the seed, `b3sum --derive-key` for the
    /// secret, `openssl pkey -pubout` for the Ed25519 public key.
    const ZERO_PHRASE_ID: &str = "610b431103deb4f932b5a063abf7d0f5d3fdcb63a6ae542283d6d4e0fd737111";

    fn portable() -> (Arc<MemoryStorage>, Portable) {
        let storage = Arc::new(MemoryStorage::default());
        (storage.clone(), Portable::new(storage))
    }

    #[test]
    fn a_generated_root_is_recovered_from_its_phrase() {
        let (root, phrase) = PortableRoot::generate();
        assert_eq!(phrase.words().len(), PHRASE_WORDS);
        let recovered = PortableRoot::recover(&phrase.to_string()).unwrap();
        assert_eq!(recovered.id(), root.id());
    }

    #[test]
    fn two_generated_roots_differ() {
        assert_ne!(
            PortableRoot::generate().0.id(),
            PortableRoot::generate().0.id()
        );
    }

    #[test]
    fn the_test_vector_holds() {
        let root = PortableRoot::recover(&zero_phrase()).unwrap();
        assert_eq!(root.id().to_string(), ZERO_PHRASE_ID);
    }

    #[test]
    fn spacing_and_case_do_not_matter() {
        let messy = format!(
            "  \t{}\n ",
            zero_phrase().to_uppercase().replace(' ', "   ")
        );
        assert_eq!(
            PortableRoot::recover(&messy).unwrap().id().to_string(),
            ZERO_PHRASE_ID
        );
    }

    #[test]
    fn a_wrong_checksum_is_told_apart() {
        // "abandon" is a word, but not the last one the checksum allows.
        let phrase = ["abandon"; 24].join(" ");
        assert_eq!(
            PortableRoot::recover(&phrase).unwrap_err(),
            PhraseError::Checksum
        );
    }

    #[test]
    fn an_unknown_word_is_named_by_position() {
        let phrase = zero_phrase().replacen("abandon", "abandn", 1);
        assert_eq!(
            PortableRoot::recover(&phrase).unwrap_err(),
            PhraseError::UnknownWord { position: 1 }
        );
        let mut words: Vec<_> = zero_phrase().split(' ').map(str::to_owned).collect();
        words[6] = "spoolframe".into();
        assert_eq!(
            PortableRoot::recover(&words.join(" ")).unwrap_err(),
            PhraseError::UnknownWord { position: 7 }
        );
    }

    #[test]
    fn a_short_phrase_is_refused_for_its_length() {
        // A valid 12-word BIP-39 phrase is still not a Spoolframe one.
        let twelve = format!("{} about", ["abandon"; 11].join(" "));
        assert_eq!(
            PortableRoot::recover(&twelve).unwrap_err(),
            PhraseError::WordCount { found: 12 }
        );
        assert_eq!(
            PortableRoot::recover("").unwrap_err(),
            PhraseError::WordCount { found: 0 }
        );
    }

    #[test]
    fn debug_shows_no_secret() {
        let (root, phrase) = PortableRoot::generate();
        let root_debug = format!("{root:?}");
        assert!(root_debug.contains(&root.id().to_string()), "{root_debug}");
        let secret_hex: String = root.secret().iter().map(|b| format!("{b:02x}")).collect();
        assert!(!root_debug.contains(&secret_hex), "{root_debug}");
        assert!(!root_debug.contains(&format!("{:?}", root.secret().as_ref())));

        // The whole of it: a check that no word appears in it fails
        // whenever the phrase has one inside "redacted" ("act", "red").
        assert_eq!(format!("{phrase:?}"), "RecoveryPhrase(<redacted>)");
    }

    #[test]
    fn the_root_signs_what_its_id_verifies() {
        let root = PortableRoot::recover(&zero_phrase()).unwrap();
        let signature = root.sign(b"a device credential");
        assert!(root.id().verify(b"a device credential", &signature));
        assert!(!root.id().verify(b"another credential", &signature));
        let (other, _) = PortableRoot::generate();
        assert!(!other.id().verify(b"a device credential", &signature));
    }

    #[tokio::test]
    async fn a_created_identity_is_listed_and_its_root_kept() {
        let (_, portable) = portable();
        let (id, phrase) = portable.create("Steve").await.unwrap();
        let listed = portable.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].id, listed[0].name.as_str()), (id, "Steve"));
        assert_eq!(portable.load_root(id).await.unwrap().id(), id);
        assert_eq!(PortableRoot::recover(&phrase.to_string()).unwrap().id(), id);
    }

    #[tokio::test]
    async fn recovering_on_a_fresh_install_gives_the_same_identity() {
        let (_, phone) = portable();
        let (id, phrase) = phone.create("Steve").await.unwrap();
        let (_, fresh) = portable();
        assert_eq!(fresh.recover(&phrase.to_string(), "Me").await.unwrap(), id);
        assert_eq!(fresh.load_root(id).await.unwrap().id(), id);
    }

    #[tokio::test]
    async fn recovering_an_identity_already_here_keeps_one_entry() {
        let (_, portable) = portable();
        let first = portable.recover(&zero_phrase(), "First").await.unwrap();
        let again = portable.recover(&zero_phrase(), "Again").await.unwrap();
        assert_eq!(first, again);
        let listed = portable.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "First");
    }

    #[tokio::test]
    async fn a_bad_phrase_stores_nothing() {
        let (storage, portable) = portable();
        let err = portable
            .recover(&["abandon"; 24].join(" "), "Steve")
            .await
            .unwrap_err();
        assert_eq!(err, SdkError::Phrase(PhraseError::Checksum));
        assert_eq!(storage.data(keys::PORTABLE_IDENTITIES).await.unwrap(), None);
    }

    #[tokio::test]
    async fn two_identities_on_one_device_keep_apart() {
        let (_, portable) = portable();
        let (steve, _) = portable.create("Steve").await.unwrap();
        let ann = portable.recover(&zero_phrase(), "Ann").await.unwrap();
        assert_ne!(steve, ann);
        let listed = portable.list().await.unwrap();
        let names: Vec<_> = listed.iter().map(|e| (e.id, e.name.as_str())).collect();
        assert_eq!(names, [(steve, "Steve"), (ann, "Ann")]);
        let steve_root = portable.load_root(steve).await.unwrap();
        let ann_root = portable.load_root(ann).await.unwrap();
        assert_eq!((steve_root.id(), ann_root.id()), (steve, ann));
        let signature = steve_root.sign(b"credential");
        assert!(!ann.verify(b"credential", &signature));
    }

    #[tokio::test]
    async fn forgetting_removes_the_root_and_the_entry_only() {
        let (storage, portable) = portable();
        let (steve, _) = portable.create("Steve").await.unwrap();
        let ann = portable.recover(&zero_phrase(), "Ann").await.unwrap();
        portable.forget(steve).await.unwrap();
        let listed: Vec<_> = portable
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(listed, [ann]);
        assert_eq!(
            storage
                .secret(&keys::portable_root(&steve.to_string()))
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            portable.load_root(steve).await.unwrap_err(),
            SdkError::UnknownIdentity(steve.to_string())
        );
        assert_eq!(portable.load_root(ann).await.unwrap().id(), ann);
        // Again: nothing to forget, nothing goes wrong.
        portable.forget(steve).await.unwrap();
    }

    /// Storage whose data cannot be read.
    struct Unreadable(MemoryStorage);

    #[async_trait]
    impl Storage for Unreadable {
        async fn secret(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
            self.0.secret(key).await
        }
        async fn set_secret(&self, key: &str, value: Option<Vec<u8>>) -> Result<(), StorageError> {
            self.0.set_secret(key, value).await
        }
        async fn data(&self, _: &str) -> Result<Option<Vec<u8>>, StorageError> {
            Err(StorageError("locked".into()))
        }
        async fn set_data(&self, key: &str, value: Option<Vec<u8>>) -> Result<(), StorageError> {
            self.0.set_data(key, value).await
        }
    }

    #[tokio::test]
    async fn a_list_that_cannot_be_read_is_an_error_not_empty() {
        let portable = Portable::new(Arc::new(Unreadable(MemoryStorage::default())));
        let locked = SdkError::Storage(StorageError("locked".into()));
        assert_eq!(portable.list().await.unwrap_err(), locked);
        // Nor is it overwritten with a list of one.
        assert_eq!(portable.create("Steve").await.unwrap_err(), locked);
    }

    #[tokio::test]
    async fn a_list_that_does_not_parse_is_an_error_not_empty() {
        let (storage, portable) = portable();
        storage
            .set_data(keys::PORTABLE_IDENTITIES, Some(b"{".to_vec()))
            .await
            .unwrap();
        assert!(matches!(
            portable.list().await.unwrap_err(),
            SdkError::Storage(_)
        ));
    }

    #[tokio::test]
    async fn a_root_stored_under_another_id_is_refused() {
        let (storage, portable) = portable();
        let ann = portable.recover(&zero_phrase(), "Ann").await.unwrap();
        let (other, _) = PortableRoot::generate();
        storage
            .set_secret(
                &keys::portable_root(&ann.to_string()),
                Some(other.secret().to_vec()),
            )
            .await
            .unwrap();
        assert!(matches!(
            portable.load_root(ann).await.unwrap_err(),
            SdkError::Storage(_)
        ));
    }

    // P7-02: device credentials and identity state.

    /// 2026-10-09T00:00:00Z.
    const T: i64 = 1_791_504_000;
    const SERVER: [u8; 32] = [0x5a; 32];
    const NONCE: [u8; 32] = [0xa5; 32];

    /// A phone holding a new identity's root, and that identity.
    async fn phone() -> (Portable, PortableUserId) {
        let (_, phone) = portable();
        let (id, _) = phone.create("Steve").await.unwrap();
        (phone, id)
    }

    /// A device without the root that holds a credential for `id` from
    /// `phone`, issued at [`T`].
    async fn device_of(phone: &Portable, id: PortableUserId) -> Portable {
        let (_, device) = portable();
        let key = device.device_key(id).await.unwrap();
        let credential = phone.issue_credential_at(id, key, None, T).await.unwrap();
        device.install_credential_at(credential, T).await.unwrap();
        device
    }

    #[tokio::test]
    async fn a_device_key_is_made_once_per_identity() {
        let (phone, steve) = phone().await;
        let ann = phone.recover(&zero_phrase(), "Ann").await.unwrap();
        let first = phone.device_key(steve).await.unwrap();
        assert_eq!(phone.device_key(steve).await.unwrap(), first);
        assert_ne!(phone.device_key(ann).await.unwrap(), first);
    }

    #[tokio::test]
    async fn a_credential_installed_on_a_device_without_the_root_proves_it() {
        let (phone, id) = phone().await;
        let tv = device_of(&phone, id).await;
        assert_eq!(tv.list().await.unwrap(), []);
        let credential = tv.credential(id).await.unwrap().unwrap();
        assert_eq!(credential.user, id);
        assert_eq!(credential.device_key, tv.device_key(id).await.unwrap());
        assert_eq!(
            credential.expires_at - credential.issued_at,
            MAX_CREDENTIAL_VALIDITY
        );
        let (sent, signature) = tv.prove_at(id, &SERVER, &NONCE, T).await.unwrap();
        assert_eq!(sent, credential);
        assert_eq!(sent.verify(T), Ok(()));
        assert_eq!(
            spoolframe_protocol::portable::verify_device_proof(&sent, &SERVER, &NONCE, &signature),
            Ok(())
        );
    }

    #[tokio::test]
    async fn two_devices_hold_distinct_keys_and_credentials() {
        let (phone, id) = phone().await;
        let tv = device_of(&phone, id).await;
        let laptop = device_of(&phone, id).await;
        let tv_credential = tv.credential(id).await.unwrap().unwrap();
        let laptop_credential = laptop.credential(id).await.unwrap().unwrap();
        assert_ne!(tv_credential.device_key, laptop_credential.device_key);
        assert_ne!(tv_credential.serial, laptop_credential.serial);
        // Neither installs the other's.
        assert_eq!(
            tv.install_credential_at(laptop_credential, T)
                .await
                .unwrap_err(),
            SdkError::NotThisDevice
        );
    }

    #[tokio::test]
    async fn the_serial_counts_up_and_survives_a_restart() {
        let (storage, phone) = portable();
        let (id, _) = phone.create("Steve").await.unwrap();
        let key = phone.device_key(id).await.unwrap();
        let first = phone.issue_credential_at(id, key, None, T).await.unwrap();
        let renewed = phone
            .issue_credential_at(id, key, None, T + 1)
            .await
            .unwrap();
        assert_eq!((first.serial, renewed.serial), (1, 2));
        let restarted = Portable::new(storage);
        assert_eq!(
            restarted
                .issue_credential_at(id, key, None, T)
                .await
                .unwrap()
                .serial,
            3
        );
    }

    /// Storage that refuses to write data under keys with a prefix.
    struct RefusesWrites(MemoryStorage, &'static str);

    #[async_trait]
    impl Storage for RefusesWrites {
        async fn secret(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
            self.0.secret(key).await
        }
        async fn set_secret(&self, key: &str, value: Option<Vec<u8>>) -> Result<(), StorageError> {
            self.0.set_secret(key, value).await
        }
        async fn data(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
            self.0.data(key).await
        }
        async fn set_data(&self, key: &str, value: Option<Vec<u8>>) -> Result<(), StorageError> {
            if key.starts_with(self.1) {
                return Err(StorageError("full".into()));
            }
            self.0.set_data(key, value).await
        }
    }

    #[tokio::test]
    async fn no_credential_is_signed_unless_its_serial_is_saved() {
        let phone = Portable::new(Arc::new(RefusesWrites(
            MemoryStorage::default(),
            "portable-serial/",
        )));
        let (id, _) = phone.create("Steve").await.unwrap();
        let key = phone.device_key(id).await.unwrap();
        assert_eq!(
            phone
                .issue_credential_at(id, key, None, T)
                .await
                .unwrap_err(),
            SdkError::Storage(StorageError("full".into()))
        );
    }

    #[tokio::test]
    async fn issuing_needs_the_root() {
        let (phone, id) = phone().await;
        let tv = device_of(&phone, id).await;
        let key = tv.device_key(id).await.unwrap();
        assert_eq!(
            tv.issue_credential(id, key, None).await.unwrap_err(),
            SdkError::UnknownIdentity(id.to_string())
        );
    }

    #[tokio::test]
    async fn a_device_refuses_a_credential_that_does_not_hold() {
        let (phone, id) = phone().await;
        let (_, tv) = portable();
        let key = tv.device_key(id).await.unwrap();
        let good = phone.issue_credential_at(id, key, None, T).await.unwrap();
        let tampered = UserDeviceCredential {
            serial: good.serial + 1,
            ..good.clone()
        };
        assert_eq!(
            tv.install_credential_at(tampered, T).await.unwrap_err(),
            SdkError::Credential(CredentialError::BadSignature)
        );
        assert_eq!(
            tv.install_credential_at(good.clone(), T + MAX_CREDENTIAL_VALIDITY)
                .await
                .unwrap_err(),
            SdkError::Credential(CredentialError::Expired)
        );
        // A credential for a key this device never made for the identity.
        let (_, other) = portable();
        let elsewhere = phone
            .issue_credential_at(id, other.device_key(id).await.unwrap(), None, T)
            .await
            .unwrap();
        assert_eq!(
            tv.install_credential_at(elsewhere, T).await.unwrap_err(),
            SdkError::NotThisDevice
        );
        assert_eq!(tv.credential(id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_expired_credential_is_not_used_to_prove() {
        let (phone, id) = phone().await;
        let tv = device_of(&phone, id).await;
        assert_eq!(
            tv.prove_at(id, &SERVER, &NONCE, T + MAX_CREDENTIAL_VALIDITY)
                .await
                .unwrap_err(),
            SdkError::Credential(CredentialError::Expired)
        );
    }

    #[tokio::test]
    async fn revoking_adds_to_the_last_state_one_revision_up() {
        let (storage, phone) = portable();
        let (id, _) = phone.create("Steve").await.unwrap();
        assert_eq!(phone.identity_state(id).await.unwrap(), None);
        let tv = device_of(&phone, id).await;
        let laptop = device_of(&phone, id).await;
        let tv_key = tv.device_key(id).await.unwrap();
        let laptop_key = laptop.device_key(id).await.unwrap();

        let first = phone.revoke_at(id, tv_key, T).await.unwrap();
        let stamped = RevokedKey {
            key: tv_key,
            revoked_at: T,
        };
        assert_eq!(
            (first.revision, first.revoked.as_slice()),
            (1, &[stamped][..])
        );
        assert_eq!(first.verify(), Ok(()));
        let second = phone.revoke_at(id, laptop_key, T).await.unwrap();
        assert_eq!(second.revision, 2);
        assert!(second.revokes(&tv_key) && second.revokes(&laptop_key));
        // Again: nothing new to sign.
        assert_eq!(phone.revoke_at(id, tv_key, T).await.unwrap(), second);
        // Kept, across a restart.
        let restarted = Portable::new(storage);
        assert_eq!(restarted.identity_state(id).await.unwrap(), Some(second));
        // And a revoked key gets no new credential.
        assert_eq!(
            restarted
                .issue_credential_at(id, tv_key, None, T)
                .await
                .unwrap_err(),
            SdkError::DeviceKeyRevoked
        );
    }

    #[tokio::test]
    async fn a_device_keeps_only_a_newer_state_of_an_identity_it_knows() {
        let (phone, id) = phone().await;
        let tv = device_of(&phone, id).await;
        let tv_key = tv.device_key(id).await.unwrap();
        let (_, other) = portable();
        let first = phone
            .revoke_at(id, other.device_key(id).await.unwrap(), T)
            .await
            .unwrap();
        let second = phone.revoke_at(id, tv_key, T).await.unwrap();

        assert!(tv.accept_state(first.clone()).await.unwrap());
        assert!(tv.accept_state(second.clone()).await.unwrap());
        assert!(!tv.accept_state(first).await.unwrap());
        assert!(!tv.accept_state(second.clone()).await.unwrap());
        assert_eq!(tv.identity_state(id).await.unwrap(), Some(second));
        // The newest state revokes this device's own key: it stops.
        assert_eq!(
            tv.prove_at(id, &SERVER, &NONCE, T).await.unwrap_err(),
            SdkError::DeviceKeyRevoked
        );
    }

    #[tokio::test]
    async fn a_state_of_an_unknown_identity_or_unsigned_is_refused() {
        let (phone, id) = phone().await;
        let (_, stranger) = portable();
        let key = stranger.device_key(id).await.unwrap();
        let state = phone.revoke_at(id, key, T).await.unwrap();
        assert_eq!(
            stranger.accept_state(state.clone()).await.unwrap_err(),
            SdkError::UnknownIdentity(id.to_string())
        );
        let tv = device_of(&phone, id).await;
        let tampered = PortableIdentityState {
            revision: 9,
            ..state
        };
        assert_eq!(
            tv.accept_state(tampered).await.unwrap_err(),
            SdkError::IdentityState(StateError::BadSignature)
        );
        assert_eq!(tv.identity_state(id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn credentials_that_cannot_be_read_are_an_error_not_none() {
        let (phone, id) = phone().await;
        let tv = Portable::new(Arc::new(Unreadable(MemoryStorage::default())));
        let locked = SdkError::Storage(StorageError("locked".into()));
        assert_eq!(tv.credential(id).await.unwrap_err(), locked);
        let key = tv.device_key(id).await.unwrap();
        let credential = phone.issue_credential_at(id, key, None, T).await.unwrap();
        // Not overwritten with a list of one, either.
        assert_eq!(
            tv.install_credential_at(credential, T).await.unwrap_err(),
            locked
        );
    }

    #[tokio::test]
    async fn a_state_that_does_not_parse_is_an_error_not_none() {
        let (storage, phone) = portable();
        let (id, _) = phone.create("Steve").await.unwrap();
        storage
            .set_data(&keys::portable_state(&id.to_string()), Some(b"{".to_vec()))
            .await
            .unwrap();
        assert!(matches!(
            phone.identity_state(id).await.unwrap_err(),
            SdkError::Storage(_)
        ));
        let key = phone.device_key(id).await.unwrap();
        // Nor is a key revoked over a state that cannot be read.
        assert!(matches!(
            phone.revoke_at(id, key, T).await.unwrap_err(),
            SdkError::Storage(_)
        ));
    }

    /// A device key of nobody's, from `n`.
    fn some_key(n: u32) -> DevicePublicKey {
        let key = SigningKey::from_bytes(blake3::hash(&n.to_be_bytes()).as_bytes());
        public_key(&key)
    }

    #[tokio::test]
    async fn a_lapsed_revocation_leaves_the_state_but_not_this_root() {
        let (phone, id) = phone().await;
        let old = some_key(1);
        phone.revoke_at(id, old, T).await.unwrap();
        let later = T + REVOCATION_WINDOW + 1;
        let state = phone.revoke_at(id, some_key(2), later).await.unwrap();
        assert_eq!(state.revision, 2);
        assert_eq!(
            state.revoked,
            [RevokedKey {
                key: some_key(2),
                revoked_at: later
            }]
        );
        // The root itself still never issues to it.
        assert_eq!(
            phone
                .issue_credential_at(id, old, None, later)
                .await
                .unwrap_err(),
            SdkError::DeviceKeyRevoked
        );
    }

    #[tokio::test]
    async fn the_limit_counts_live_revocations_only() {
        let (storage, phone) = portable();
        let (id, _) = phone.create("Steve").await.unwrap();
        // As if this root had revoked the limit's worth at T.
        let full: Revoked = (0..MAX_REVOKED_KEYS).map(|n| (some_key(n), T)).collect();
        save_revoked(storage.as_ref(), id, &full).await.unwrap();
        assert_eq!(
            phone
                .revoke_at(id, some_key(MAX_REVOKED_KEYS), T)
                .await
                .unwrap_err(),
            SdkError::IdentityState(StateError::TooManyRevoked)
        );
        let later = T + REVOCATION_WINDOW + 1;
        let state = phone
            .revoke_at(id, some_key(MAX_REVOKED_KEYS), later)
            .await
            .unwrap();
        assert_eq!(state.revoked.len(), 1);
    }

    /// [`spoolframe_protocol::portable::MAX_REVOKED`], as a `u32`.
    const MAX_REVOKED_KEYS: u32 = spoolframe_protocol::portable::MAX_REVOKED as u32;

    #[tokio::test]
    async fn an_older_state_still_adds_its_revocations() {
        // Two phones hold the one root (recovered on both), and each
        // revokes a different device at revision 1.
        let (_, first_phone) = portable();
        let id = first_phone.recover(&zero_phrase(), "Ann").await.unwrap();
        let (_, second_phone) = portable();
        second_phone.recover(&zero_phrase(), "Ann").await.unwrap();
        let tv = device_of(&first_phone, id).await;
        let laptop = device_of(&first_phone, id).await;
        let tv_key = tv.device_key(id).await.unwrap();
        let laptop_key = laptop.device_key(id).await.unwrap();
        let from_first = first_phone.revoke_at(id, tv_key, T).await.unwrap();
        let from_second = second_phone.revoke_at(id, laptop_key, T).await.unwrap();
        assert_eq!((from_first.revision, from_second.revision), (1, 1));

        let (_, carrier) = portable();
        carrier.recover(&zero_phrase(), "Ann").await.unwrap();
        assert!(carrier.accept_state(from_first.clone()).await.unwrap());
        // Not newer, so not kept as the newest; its key is revoked all
        // the same.
        assert!(!carrier.accept_state(from_second.clone()).await.unwrap());
        assert_eq!(carrier.identity_state(id).await.unwrap(), Some(from_first));
        assert_eq!(
            carrier
                .issue_credential_at(id, laptop_key, None, T)
                .await
                .unwrap_err(),
            SdkError::DeviceKeyRevoked
        );
        // And its next state carries both.
        let next = carrier.revoke_at(id, some_key(9), T + 1).await.unwrap();
        assert_eq!(next.revision, 2);
        assert!(next.revokes(&tv_key) && next.revokes(&laptop_key));
        // Each revocation at its earliest.
        assert!(next.revoked.iter().all(|e| e.revoked_at <= T + 1));
    }

    #[tokio::test]
    async fn a_revoked_device_key_is_replaced() {
        let (phone, id) = phone().await;
        let tv = device_of(&phone, id).await;
        let old = tv.device_key(id).await.unwrap();
        let state = phone.revoke_at(id, old, T).await.unwrap();
        tv.accept_state(state).await.unwrap();
        let new = tv.device_key(id).await.unwrap();
        assert_ne!(new, old);
        // Kept: asked again, the same new key.
        assert_eq!(tv.device_key(id).await.unwrap(), new);
        // The old credential no longer proves; a new one for the new key
        // installs and does.
        assert_eq!(
            tv.prove_at(id, &SERVER, &NONCE, T).await.unwrap_err(),
            SdkError::DeviceKeyRevoked
        );
        let renewed = phone.issue_credential_at(id, new, None, T).await.unwrap();
        tv.install_credential_at(renewed, T).await.unwrap();
        tv.prove_at(id, &SERVER, &NONCE, T).await.unwrap();
    }

    #[tokio::test]
    async fn revoked_keys_that_cannot_be_read_are_an_error_not_none() {
        let (storage, phone) = portable();
        let (id, _) = phone.create("Steve").await.unwrap();
        let key = phone.device_key(id).await.unwrap();
        storage
            .set_data(
                &keys::portable_revoked(&id.to_string()),
                Some(b"[".to_vec()),
            )
            .await
            .unwrap();
        assert!(matches!(
            phone
                .issue_credential_at(id, key, None, T)
                .await
                .unwrap_err(),
            SdkError::Storage(_)
        ));
        assert!(matches!(
            phone.device_key(id).await.unwrap_err(),
            SdkError::Storage(_)
        ));
    }

    #[tokio::test]
    async fn issued_lists_each_device_once_with_its_newest_credential() {
        let (storage, phone) = portable();
        let (id, _) = phone.create("Steve").await.unwrap();
        assert_eq!(phone.issued(id).await.unwrap(), []);
        let tv = some_key(1);
        let laptop = some_key(2);
        phone
            .issue_credential_at(id, tv, Some("Living room"), T)
            .await
            .unwrap();
        phone
            .issue_credential_at(id, laptop, Some("Laptop"), T)
            .await
            .unwrap();
        // A renewal without a label keeps the one before.
        let renewed = phone
            .issue_credential_at(id, tv, None, T + 5)
            .await
            .unwrap();
        let issued = Portable::new(storage).issued(id).await.unwrap();
        let seen: Vec<_> = issued
            .iter()
            .map(|e| (e.device_key, e.serial, e.issued_at, e.label.as_deref()))
            .collect();
        assert_eq!(
            seen,
            [
                (tv, 3, T + 5, Some("Living room")),
                (laptop, 2, T, Some("Laptop"))
            ]
        );
        assert_eq!(issued[0].expires_at, renewed.expires_at);
        // A renewal with a label takes it.
        phone
            .issue_credential_at(id, laptop, Some("Old laptop"), T + 6)
            .await
            .unwrap();
        assert_eq!(
            phone.issued(id).await.unwrap()[1].label.as_deref(),
            Some("Old laptop")
        );
    }

    #[tokio::test]
    async fn issued_says_which_devices_are_revoked() {
        let (phone, id) = phone().await;
        phone
            .issue_credential_at(id, some_key(1), None, T)
            .await
            .unwrap();
        phone
            .issue_credential_at(id, some_key(2), None, T)
            .await
            .unwrap();
        phone.revoke_at(id, some_key(1), T + 7).await.unwrap();
        let revoked: Vec<_> = phone
            .issued(id)
            .await
            .unwrap()
            .iter()
            .map(|e| e.revoked_at)
            .collect();
        assert_eq!(revoked, [Some(T + 7), None]);
    }

    #[tokio::test]
    async fn no_credential_is_given_out_unless_its_device_is_recorded() {
        let phone = Portable::new(Arc::new(RefusesWrites(
            MemoryStorage::default(),
            "portable-issued/",
        )));
        let (id, _) = phone.create("Steve").await.unwrap();
        assert_eq!(
            phone
                .issue_credential_at(id, some_key(1), None, T)
                .await
                .unwrap_err(),
            SdkError::Storage(StorageError("full".into()))
        );
    }

    #[tokio::test]
    async fn an_identity_whose_root_is_gone_is_listed_without_it() {
        let (storage, phone) = portable();
        let (steve, _) = phone.create("Steve").await.unwrap();
        let ann = phone.recover(&zero_phrase(), "Ann").await.unwrap();
        // As a restore that left out this-device-only secrets would.
        storage
            .set_secret(&keys::portable_root(&steve.to_string()), None)
            .await
            .unwrap();
        let has_root: Vec<_> = phone
            .list()
            .await
            .unwrap()
            .iter()
            .map(|e| (e.id, e.has_root))
            .collect();
        assert_eq!(has_root, [(steve, false), (ann, true)]);
    }

    // P7-04: the proof that ties a local account.

    #[tokio::test]
    async fn the_phone_ties_with_its_root() {
        let (phone, id) = phone().await;
        let message = tie_message(&SERVER, &NONCE, &id, "alice").unwrap();
        let proof = phone
            .tie_proof_at(id, &SERVER, &NONCE, "alice", T)
            .await
            .unwrap();
        assert!(matches!(proof, TieProof::Root { .. }));
        assert_eq!(proof.verify(&id, &message, T), Ok(()));
        // Made for alice, it is no proof for bob.
        let for_bob = tie_message(&SERVER, &NONCE, &id, "bob").unwrap();
        assert!(proof.verify(&id, &for_bob, T).is_err());
    }

    #[tokio::test]
    async fn a_device_without_the_root_ties_with_its_credential() {
        let (phone, id) = phone().await;
        let tv = device_of(&phone, id).await;
        let message = tie_message(&SERVER, &NONCE, &id, "alice").unwrap();
        let proof = tv
            .tie_proof_at(id, &SERVER, &NONCE, "alice", T)
            .await
            .unwrap();
        match &proof {
            TieProof::Device { credential, .. } => {
                assert_eq!(credential.device_key, tv.device_key(id).await.unwrap());
            }
            other => panic!("not a device proof: {other:?}"),
        }
        assert_eq!(proof.verify(&id, &message, T), Ok(()));
    }

    #[tokio::test]
    async fn a_revoked_or_expired_device_does_not_tie() {
        let (phone, id) = phone().await;
        let tv = device_of(&phone, id).await;
        assert_eq!(
            tv.tie_proof_at(id, &SERVER, &NONCE, "alice", T + MAX_CREDENTIAL_VALIDITY)
                .await
                .unwrap_err(),
            SdkError::Credential(CredentialError::Expired)
        );
        let key = tv.device_key(id).await.unwrap();
        let state = phone.revoke_at(id, key, T).await.unwrap();
        tv.accept_state(state).await.unwrap();
        assert_eq!(
            tv.tie_proof_at(id, &SERVER, &NONCE, "alice", T)
                .await
                .unwrap_err(),
            SdkError::DeviceKeyRevoked
        );
    }

    #[tokio::test]
    async fn an_identity_this_device_cannot_act_as_does_not_tie() {
        let (_phone, id) = phone().await;
        let (_, stranger) = portable();
        assert_eq!(
            stranger
                .tie_proof_at(id, &SERVER, &NONCE, "alice", T)
                .await
                .unwrap_err(),
            SdkError::UnknownIdentity(id.to_string())
        );
    }
}
