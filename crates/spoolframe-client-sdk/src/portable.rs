//! Portable identities: a root signing key
//! the user owns, its [`PortableUserId`], and the recovery phrase that
//! gives the same key back on any device.
//!
//! The phone app holds the root. It is signing-only: it authorizes
//! device credentials and is never itself sent anywhere.
//!
//! Device credentials: each device that acts as an
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

mod heads;
mod key_roots;
mod sync;

pub use sync::{RootLocation, RootSyncRefused};

pub use heads::MAX_HEADS;
use heads::{load_heads, save_heads, take_in};

pub use key_roots::{
    AddedRoot, GenesisError, HANDLE_LEN, IdentityGenesis, IdentityRecord, KeyAssertion,
    PORTABLE_RP, RENEWAL_DUE, RemovedRoot, RootKey, RootRegistration, RootSet, RootSignature,
    RootSigner, SecurityKey, SecurityKeyError, SoftKey, SoftSecurityKey, StateChanges,
    WebAuthnError,
};

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
    let normalized = normalized(phrase);
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

/// `phrase` trimmed, single-spaced and lower case, written straight into
/// one zeroized buffer: no word is copied anywhere else on the heap,
/// and the buffer is sized first, so it never grows and leaves a freed
/// copy behind.
fn normalized(phrase: &str) -> Zeroizing<String> {
    fn lower(word: &str) -> impl Iterator<Item = char> + '_ {
        word.chars().flat_map(char::to_lowercase)
    }
    let len: usize = phrase
        .split_whitespace()
        .map(|word| lower(word).map(char::len_utf8).sum::<usize>() + 1)
        .sum();
    let mut out = Zeroizing::new(String::with_capacity(len));
    let start = out.as_ptr();
    for word in phrase.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(lower(word));
    }
    debug_assert_eq!(out.as_ptr(), start, "the phrase's buffer grew");
    out
}

/// A guest on this device ([`Portable::guests`]), with where and when it
/// was last paired onto it ([`Portable::guest_pairings`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestPairing {
    pub id: PortableUserId,
    /// The server the pairing was for: the one a pairing through a server
    /// went through, or the first server a QR pairing's phone handed
    /// ([`crate::Paired::servers`]). `None` when there was none, or none
    /// is recorded.
    pub server: Option<GuestServer>,
    /// When it was last paired onto this device, Unix seconds. `None` when
    /// none is recorded.
    pub paired_at: Option<u64>,
}

/// The server a guest was paired for ([`GuestPairing::server`]), as this
/// device knew it then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestServer {
    /// Its key, 64 hex digits: a saved server's
    /// [`crate::ServerEntry::endpoint_id`].
    pub endpoint_id: String,
    /// Its name, as saved or as it said in its Hello at the time.
    pub name: String,
}

/// What [`keys::portable_paired`] holds.
#[derive(Serialize, Deserialize)]
struct PairedRecord {
    server: Option<GuestServer>,
    paired_at: u64,
}

/// Which of this device's keys a credential being installed is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceKey {
    /// The one kept for its identity ([`Portable::device_key`]).
    Kept,
    /// The pending one made for a pairing through a server, before the
    /// identity was known: adopted as the identity's once the credential
    /// is accepted.
    Pending,
}

/// The longest name [`Portable::rename`] takes, in characters, once
/// trimmed. Generous for a person's name or a label like "Work", short
/// enough for a list row.
pub const MAX_IDENTITY_NAME: usize = 64;

/// A portable identity this device holds the root of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableIdentity {
    pub id: PortableUserId,
    /// What the user calls it ([`Portable::rename`]); theirs alone, never
    /// sent to a server. With its root synced it goes, in the synced
    /// record, to the user's own devices only, and the newest name
    /// ([`Self::name_set_at`]) is the one every device lists.
    pub name: String,
    /// Unix seconds: when [`Self::name`] was last set by a rename, here or
    /// on another synced device. 0 for the name it was made, recovered or
    /// adopted with, so any rename beats that one.
    // Read as the default when left out: a stored record, not a message of
    // the client protocol, and one written by an SDK before it lacks the key.
    #[serde(default)]
    pub name_set_at: u64,
    /// Unix seconds: made, or first recovered, here.
    pub created_at: u64,
    /// Whether its root is here now, as [`Portable::list`] found it: this
    /// device's own copy, or the synced one ([`Portable::root_location`]).
    /// Not stored: an app's storage restored from a backup that leaves
    /// out secrets (the iOS keychain's this-device-only items) lists the
    /// identity without its own copy, and a synced root can be deleted
    /// from another device. Without it nothing is issued, revoked or tied
    /// with the root; recovering from the phrase puts it back.
    #[serde(skip)]
    pub has_root: bool,
    /// Rooted in security keys: its genesis is kept here, and its
    /// root actions take a key ([`RootSigner::Key`]). A phrase identity
    /// that added a key is not one: its phrase is still a root.
    // Read as the default when left out: a stored record, not a message of
    // the client protocol, and one written by an SDK before it lacks the key.
    #[serde(default)]
    pub key_rooted: bool,
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
            entry.has_root = self.has_root(entry.id).await?;
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
        let heads = load_heads(storage, id).await?;
        for entry in &mut issued {
            entry.revoked_at = revoked.get(&entry.device_key).copied().or_else(|| {
                heads
                    .iter()
                    .flat_map(|s| &s.revoked)
                    .find(|r| r.key == entry.device_key)
                    .map(|r| r.revoked_at)
            });
        }
        Ok(issued)
    }

    /// Remove identity `id` from this device: its root, its entry, and
    /// this device's key and credential for it, so the device no longer
    /// signs in as it (nor lists it as a guest). Its phrase still
    /// recovers it. What its root issued and revoked here, and its serial,
    /// stay: a recovered root goes on from them. Forgetting one not here
    /// does nothing.
    ///
    /// **A synced root stays.** Only this device's data goes, never the
    /// synced root nor the synced record ([`keys::portable_root_synced`],
    /// [`keys::portable_synced`]): deleting those would delete them on
    /// every device the app syncs to. The user's other devices keep the
    /// identity; this one stops acting as it, and
    /// [`Portable::adopt_synced`] lists it again. To stop syncing, use
    /// [`Portable::disable_root_sync`].
    pub async fn forget(&self, id: PortableUserId) -> Result<(), SdkError> {
        let storage = self.storage.as_ref();
        let _saving = self.saving.lock().await;
        // The credential first, as forget_guest does: should the rest
        // fail, nothing acts as the identity, and forgetting again clears
        // what is left.
        let mut credentials = load_credentials(storage).await?;
        if credentials.remove(&id).is_some() {
            save_credentials(storage, &credentials).await?;
        }
        storage
            .set_secret(&keys::portable_device(&id.to_string()), None)
            .await?;
        storage
            .set_data(&keys::portable_paired(&id.to_string()), None)
            .await?;
        // The root before the entry: should the list then fail to save,
        // an entry with no root stays, which forgetting again clears,
        // rather than a root nothing lists.
        storage
            .set_secret(&keys::portable_root(&id.to_string()), None)
            .await?;
        let mut saved = load(storage).await?;
        saved.retain(|entry| entry.id != id);
        save(storage, &saved).await?;
        tracing::info!(%id, "forgot a portable identity");
        Ok(())
    }

    /// Call identity `id` `name` (trimmed) from now on. Needs no root and
    /// signs nothing: the name is the user's own and goes to no server.
    /// With its root synced, the new name goes to the user's other
    /// devices in the synced record, and the newest rename wins there.
    /// An empty name, or one longer than [`MAX_IDENTITY_NAME`]
    /// characters, is [`SdkError::InvalidInput`]; one not listed here is
    /// [`SdkError::UnknownIdentity`]. The name it has already changes
    /// nothing.
    pub async fn rename(&self, id: PortableUserId, name: &str) -> Result<(), SdkError> {
        self.rename_at(id, name, now()).await
    }

    async fn rename_at(&self, id: PortableUserId, name: &str, now: u64) -> Result<(), SdkError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(SdkError::InvalidInput("the name is empty".into()));
        }
        if name.chars().count() > MAX_IDENTITY_NAME {
            return Err(SdkError::InvalidInput(format!(
                "the name is longer than {MAX_IDENTITY_NAME} characters"
            )));
        }
        // The synced record first: a rename made on another device is
        // the one this rename has to be newer than.
        self.sync_data(id).await?;
        let storage = self.storage.as_ref();
        let saving = self.saving.lock().await;
        let mut saved = load(storage).await?;
        let entry = saved
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| SdkError::UnknownIdentity(id.to_string()))?;
        if entry.name == name {
            return Ok(());
        }
        entry.name = name.to_owned();
        // Newer than the last, even with the clock behind it.
        entry.name_set_at = (entry.name_set_at + 1).max(now);
        save(storage, &saved).await?;
        // sync_data takes the lock itself.
        drop(saving);
        self.sync_data(id).await?;
        tracing::info!(%id, "renamed a portable identity");
        Ok(())
    }

    /// Identity `id`'s root, for signing credentials and states: this
    /// device's copy, else the synced one ([`keys::portable_root_synced`]).
    pub(crate) async fn load_root(&self, id: PortableUserId) -> Result<PortableRoot, SdkError> {
        let bytes = load_root_bytes(self.storage.as_ref(), id).await?;
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

    // Root side: the phone that holds identity `id`'s root.

    /// A credential from identity `id`'s root for `device_key`, good for
    /// [`MAX_CREDENTIAL_VALIDITY`] from now. Renewal is the same call: a
    /// new credential for the same key, with a larger serial: the issue
    /// time in milliseconds, or one past the last here if that is larger.
    /// The serial is saved before the credential is signed, so a failure
    /// in between skips one rather than giving two credentials the same
    /// serial.
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
        // A synced root acts on what its other holders did: their revoked
        // keys and root changes first.
        self.sync_data(id).await?;
        // A phrase root the identity removed is kept here, but
        // signs nothing more: devices refuse what it signs.
        if !self.roots(id).await?.holds(&RootKey::phrase_of(&id)) {
            return Err(SdkError::NotARoot);
        }
        let _saving = self.saving.lock().await;
        if is_revoked(storage, id, &device_key).await? {
            return Err(SdkError::DeviceKeyRevoked);
        }
        let serial = next_serial(storage, id, now).await?;
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
        drop(_saving);
        self.sync_data(id).await?;
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
        // Without the root here this is UnknownIdentity, before anything
        // else is read.
        self.load_root(id).await?;
        // A key the only head revokes already: it, unchanged. With a rival
        // head, a new state merges them.
        if let [previous] = load_heads(self.storage.as_ref(), id).await?.as_slice()
            && previous.revokes(&device_key)
        {
            return Ok(previous.clone());
        }
        // Through the same path as `revoke_by`, which carries the roots
        // added and removed forward: a state built from the revocations
        // alone would drop a security key the phrase identity added.
        let state = self
            .new_state(
                id,
                RootSigner::Phrase,
                None,
                |changes| {
                    // Revoked from now, unless it already is (from a state
                    // accepted from elsewhere) and that revocation has not
                    // lapsed: lapsed ones are not among `changes.revoked`.
                    if !changes.revoked.iter().any(|r| r.key == device_key) {
                        changes.revoked.push(RevokedKey {
                            key: device_key,
                            revoked_at: now,
                        });
                    }
                },
                now,
            )
            .await?;
        tracing::info!(%id, %device_key, revision = state.revision, "revoked a portable device key");
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

    // Device side: any device that acts as an identity, the
    // root's own phone or a TV that never holds it.

    /// This device's key for acting as identity `id`, made and kept the
    /// first time it is asked for. Show it to the root's phone (by the
    /// pairing QR code) to get a credential for it. A key revoked here is replaced by
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
        self.install_credential_for(credential, now, DeviceKey::Kept)
            .await
    }

    /// [`Self::install_credential_at`] for the device key `key` names. A
    /// pending key ([`DeviceKey::Pending`], a pairing through a server)
    /// is checked as the kept one is, and becomes the identity's only
    /// once every check passed, with the credential: a refused credential
    /// leaves the key this device had, which its credential names.
    pub(crate) async fn install_credential_for(
        &self,
        credential: UserDeviceCredential,
        now: i64,
        key: DeviceKey,
    ) -> Result<(), SdkError> {
        credential.verify(now)?;
        let id = credential.user;
        self.check_signer(&credential).await?;
        let _saving = self.saving.lock().await;
        let storage = self.storage.as_ref();
        let ours = match key {
            DeviceKey::Kept => load_device_key(storage, id).await?,
            DeviceKey::Pending => key_roots::load_pending(storage).await?,
        };
        let Some(ours) = ours.filter(|k| public_key(k) == credential.device_key) else {
            return Err(SdkError::NotThisDevice);
        };
        if is_revoked(storage, id, &credential.device_key).await? {
            return Err(SdkError::DeviceKeyRevoked);
        }
        if key == DeviceKey::Pending {
            storage
                .set_secret(
                    &keys::portable_device(&id.to_string()),
                    Some(Zeroizing::new(ours.to_bytes()).to_vec()),
                )
                .await?;
            storage
                .set_secret(keys::PORTABLE_PENDING_DEVICE, None)
                .await?;
        }
        let mut credentials = load_credentials(storage).await?;
        credentials.insert(id, credential);
        save_credentials(storage, &credentials).await?;
        tracing::info!(%id, "installed a portable device credential");
        Ok(())
    }

    /// Whether `credential`'s signer is one of its identity's roots, as
    /// this device knows them: from the genesis it carries or the
    /// one kept here, and the newest state here. A key identity's
    /// genesis is kept from it.
    async fn check_signer(&self, credential: &UserDeviceCredential) -> Result<(), SdkError> {
        let id = credential.user;
        let storage = self.storage.as_ref();
        let carried = credential.genesis.as_deref();
        if let Some(genesis) = carried {
            genesis
                .verify()
                .map_err(|e| SdkError::InvalidInput(e.to_string()))?;
            if genesis.user() != id {
                return Err(SdkError::InvalidInput(
                    "the credential's genesis is another identity's".into(),
                ));
            }
        }
        let kept = key_roots::load_genesis(storage, id).await?;
        let genesis = carried.or(kept.as_ref());
        let states = load_heads(storage, id).await?;
        let roots = spoolframe_protocol::portable::root_set(&id, genesis, &states);
        if !roots.holds(&credential.signer()) {
            // A credential from elsewhere, not a key in hand: not NotARoot.
            return Err(CredentialError::BadSignature.into());
        }
        if let Some(genesis) = carried
            && kept.is_none()
        {
            storage
                .set_data(
                    &keys::portable_genesis(&id.to_string()),
                    Some(serde_json::to_vec(genesis).expect("a genesis serializes")),
                )
                .await?;
        }
        Ok(())
    }

    /// The credential this device acts as identity `id` with, if any.
    pub async fn credential(
        &self,
        id: PortableUserId,
    ) -> Result<Option<UserDeviceCredential>, SdkError> {
        Ok(load_credentials(self.storage.as_ref()).await?.remove(&id))
    }

    /// This device's credential for identity `id`, whose root it holds:
    /// the phone signs one for its own key, as it signs a TV's,
    /// to sign in to servers and redeem invites as `id` with the proof
    /// any device gives. The one it has is kept while it holds for
    /// [`OWN_CREDENTIAL_RENEW`] more; else a new one is issued (recorded
    /// as "This device" in [`Portable::issued`]) and installed. Without
    /// the root, the credential this device was given, if it still holds.
    pub async fn own_credential(
        &self,
        id: PortableUserId,
    ) -> Result<UserDeviceCredential, SdkError> {
        self.own_credential_at(id, unix_now()).await
    }

    async fn own_credential_at(
        &self,
        id: PortableUserId,
        now: i64,
    ) -> Result<UserDeviceCredential, SdkError> {
        let key = self.device_key(id).await?;
        let current = self
            .credential(id)
            .await?
            .filter(|c| c.device_key == key && c.verify(now).is_ok());
        if let Some(credential) = &current
            && credential.expires_at - now > OWN_CREDENTIAL_RENEW
        {
            return Ok(credential.clone());
        }
        match self
            .issue_credential_at(id, key, Some(OWN_DEVICE_LABEL), now)
            .await
        {
            Ok(credential) => {
                self.install_credential_at(credential.clone(), now).await?;
                Ok(credential)
            }
            // Not the root's phone: what it was given, while it holds.
            Err(SdkError::UnknownIdentity(_)) if current.is_some() => Ok(current.expect("checked")),
            Err(e) => Err(e),
        }
    }

    /// Take in `state`, a state another device brought: its revocations
    /// join the ones here whatever its revision (revocation only grows),
    /// and it is kept as a head, to carry on to servers, unless the heads
    /// here cover it ([`Portable::identity_states`]): the newest if its
    /// revision is the highest, else a rival that says something the
    /// heads do not (a root removed on another holder's branch). Whether
    /// it was kept. A device that holds the phrase root, still a root,
    /// then merges a rival straight away: one state over both, at one
    /// past the highest ([`Portable::reconcile`]). Only for identities
    /// this device knows (holds the root of, or a credential for); a
    /// state for another is [`SdkError::UnknownIdentity`].
    pub async fn accept_state(&self, state: PortableIdentityState) -> Result<bool, SdkError> {
        state.verify()?;
        let id = state.user;
        let storage = self.storage.as_ref();
        // Signed by one of the identity's roots, ever.
        let genesis = key_roots::load_genesis(storage, id).await?;
        let mut held = load_heads(storage, id).await?;
        let before = spoolframe_protocol::portable::root_set(&id, genesis.as_ref(), &held);
        held.push(state.clone());
        if !spoolframe_protocol::portable::root_set(&id, genesis.as_ref(), &held)
            .knew(&state.signer())
        {
            // A state from elsewhere, not a key in hand: not NotARoot.
            return Err(StateError::BadSignature.into());
        }
        // A root removed here adds no one new from now on. The state is
        // refused, but it is one of the identity's roots' word, and its
        // revocations are kept all the same: refusing them would let a
        // rival branch's revocation go unheard here.
        let refused = before.refuses_additions(&state.roots_added);
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
        if refused {
            drop(_saving);
            self.sync_data(id).await?;
            tracing::warn!(%id, revision = state.revision, "refused a portable identity state that adds a root through a removed one; kept its revocations");
            return Err(StateError::BadSignature.into());
        }
        let Some(heads) = take_in(&load_heads(storage, id).await?, &state, unix_now()) else {
            drop(_saving);
            self.sync_data(id).await?;
            return Ok(false);
        };
        save_heads(storage, id, &heads).await?;
        drop(_saving);
        self.sync_data(id).await?;
        tracing::info!(%id, revision = state.revision, heads = heads.len(), "accepted a portable identity state");
        if load_heads(storage, id).await?.len() > 1 {
            self.merge_heads_if_root_here(id).await?;
        }
        Ok(true)
    }

    /// What this device sends to sign in as identity `id` on the server
    /// whose endpoint id is `server`, answering its `nonce`: the
    /// credential, and the device key's signature over
    /// [`device_proof_message`]. [`crate::Client::sign_in_portable`] sends
    /// them.
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
    /// id is `server` to identity `id`, answering its `nonce`: the
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
            // A phrase root the identity removed signs no tie: the
            // device's credential, from a root it still has, does.
            Ok(root) if self.roots(id).await?.holds(&RootKey::phrase_of(&id)) => {
                return Ok(TieProof::Root {
                    signature: root.sign(&message).into(),
                    root: None,
                    genesis: None,
                });
            }
            Ok(_) | Err(SdkError::UnknownIdentity(_)) => {}
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

    // Guests: identities this device acts as without holding
    // their root, by the device's own leave.

    /// Whether this device lets people sign in on it as an account that
    /// is not its own: off unless the user turned it on. The shared
    /// living-room TV turns it on; a phone or a laptop keeps it off. Off,
    /// the device shows no pairing code and takes no pairing
    /// ([`crate::Client::start_pairing`]), before anything is sent
    /// anywhere.
    ///
    /// The one gate for every external principal, such as a portable
    /// identity paired by QR. Storage that cannot be read is an error, never "on".
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

    /// [`Portable::guests`], each with the server it was paired for and
    /// when: for a "Shared computer" row, "server name · date". A guest
    /// with no pairing recorded has `None` for both.
    pub async fn guest_pairings(&self) -> Result<Vec<GuestPairing>, SdkError> {
        let storage = self.storage.as_ref();
        let mut listed = Vec::new();
        for id in self.guests().await? {
            let key = keys::portable_paired(&id.to_string());
            let record = match storage.data(&key).await? {
                None => None,
                Some(bytes) => {
                    Some(serde_json::from_slice::<PairedRecord>(&bytes).map_err(|e| {
                        StorageError(format!("the pairing of guest {id} cannot be read: {e}"))
                    })?)
                }
            };
            listed.push(GuestPairing {
                id,
                server: record.as_ref().and_then(|r| r.server.clone()),
                paired_at: record.map(|r| r.paired_at),
            });
        }
        Ok(listed)
    }

    /// Guest `id` was paired onto this device just now, for `server` if
    /// one was known: kept for [`Portable::guest_pairings`], in place of
    /// an earlier pairing's.
    pub(crate) async fn note_paired(
        &self,
        id: PortableUserId,
        server: Option<GuestServer>,
    ) -> Result<(), SdkError> {
        let record = PairedRecord {
            server,
            paired_at: now(),
        };
        let bytes = serde_json::to_vec(&record).expect("a pairing record serializes");
        self.storage
            .set_data(&keys::portable_paired(&id.to_string()), Some(bytes))
            .await?;
        Ok(())
    }

    /// The identities this device holds a credential for but not the root
    /// of: the guests paired onto it, oldest id first. Each signs out on
    /// its own ([`Portable::forget_guest`]). [`Portable::guest_pairings`]
    /// says where and when each was paired.
    pub async fn guests(&self) -> Result<Vec<PortableUserId>, SdkError> {
        let storage = self.storage.as_ref();
        let mut guests = Vec::new();
        for id in load_credentials(storage).await?.into_keys() {
            if !self.has_root(id).await? {
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
        if self.has_root(id).await? {
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
        storage
            .set_data(&keys::portable_paired(&id.to_string()), None)
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
                name_set_at: 0,
                created_at: now(),
                has_root: true,
                key_rooted: false,
            });
            save(storage, &saved).await?;
        }
        Ok(())
    }
}

/// The stored bytes of `id`'s root: this device's copy, else the synced
/// one, for an identity listed here (a forgotten one does not act through
/// the synced root until adopted again). Neither is
/// [`SdkError::UnknownIdentity`].
async fn load_root_bytes(
    storage: &dyn Storage,
    id: PortableUserId,
) -> Result<Zeroizing<Vec<u8>>, SdkError> {
    if let Some(bytes) = storage
        .secret(&keys::portable_root(&id.to_string()))
        .await?
    {
        return Ok(Zeroizing::new(bytes));
    }
    if load(storage).await?.iter().any(|entry| entry.id == id)
        && let Some(bytes) = storage
            .secret(&keys::portable_root_synced(&id.to_string()))
            .await?
    {
        return Ok(Zeroizing::new(bytes));
    }
    Err(SdkError::UnknownIdentity(id.to_string()))
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

/// Whether `key` is revoked here for `id`: in the union, in a head, or in
/// the synced record ([`keys::portable_synced`]), read as it is now, so a
/// holder of a synced root never issues to a key another holder revoked,
/// however long ago.
async fn is_revoked(
    storage: &dyn Storage,
    id: PortableUserId,
    key: &DevicePublicKey,
) -> Result<bool, StorageError> {
    Ok(load_revoked(storage, id).await?.contains_key(key)
        || load_heads(storage, id)
            .await?
            .iter()
            .any(|state| state.revokes(key))
        || sync::synced_revoked(storage, id).await?.contains_key(key))
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

/// The serial for a credential `id`'s root issues at `now` (Unix seconds),
/// saved before it is used, so a failure in between skips one rather
/// than giving two credentials the same serial. It is the issue time in
/// milliseconds, or one past the last serial here if that is larger: it
/// grows on one holder whatever its clock does, and two holders of one
/// root (a synced phrase root) almost never give the same one, with no
/// count shared between them. Nothing checks a serial but its signature;
/// the device key tells credentials apart.
async fn next_serial(
    storage: &dyn Storage,
    id: PortableUserId,
    now: i64,
) -> Result<u64, StorageError> {
    let serial_key = keys::portable_serial(&id.to_string());
    let last = match storage.data(&serial_key).await? {
        None => 0,
        Some(bytes) => serde_json::from_slice::<u64>(&bytes).map_err(|e| {
            StorageError(format!("the credential serial of {id} cannot be read: {e}"))
        })?,
    };
    let next = last
        .checked_add(1)
        .ok_or_else(|| StorageError(format!("the credential serial of {id} is at its end")))?;
    let millis = u64::try_from(now).unwrap_or(0).saturating_mul(1000);
    let serial = next.max(millis);
    storage
        .set_data(&serial_key, Some(serial.to_string().into_bytes()))
        .await?;
    Ok(serial)
}

/// Unix seconds now, as credentials count them.
/// How long before its own credential expires a root's phone signs
/// itself a new one ([`Portable::own_credential`]): 30 days.
pub const OWN_CREDENTIAL_RENEW: i64 = 30 * 24 * 3600;

/// What [`Portable::issued`] calls the root's phone's own credential.
pub const OWN_DEVICE_LABEL: &str = "This device";

/// Unix seconds, by this device's clock.
pub(crate) fn unix_now() -> i64 {
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

/// The newest state of `id` here (the first of its heads); unreadable is
/// an error, never "none".
async fn load_state(
    storage: &dyn Storage,
    id: PortableUserId,
) -> Result<Option<PortableIdentityState>, StorageError> {
    Ok(load_heads(storage, id).await?.into_iter().next())
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

    /// P8: the buffer is sized before it is written, even for a letter
    /// whose lower case is longer than it ("İ", 2 bytes, is "i̇", 3), so
    /// it never grows and leaves an unzeroized copy of the phrase behind.
    #[test]
    fn a_phrase_is_normalized_into_a_buffer_that_never_grows() {
        let out = normalized("  ABANDON\t\u{130}x \n art ");
        assert_eq!(out.as_str(), "abandon i\u{307}x art");
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
    async fn forgetting_removes_one_identity_and_leaves_the_others() {
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

    /// P4: a forgotten identity's device key and credential go with its
    /// root: this device no longer signs in as it, nor lists it as a
    /// guest. What its root issued and revoked here stays, as its serial
    /// does, should the phrase recover it.
    #[tokio::test]
    async fn a_forgotten_identity_no_longer_signs_in_here() {
        let (storage, phone) = portable();
        let (id, _) = phone.create("Steve").await.unwrap();
        let now = unix_now();
        phone.own_credential_at(id, now).await.unwrap();
        assert!(phone.prove_at(id, &[1; 32], &[2; 32], now).await.is_ok());
        let issued = phone.issued(id).await.unwrap();
        let serial = keys::portable_serial(&id.to_string());
        let serial_before = storage.data(&serial).await.unwrap();

        phone.forget(id).await.unwrap();
        assert_eq!(
            phone
                .prove_at(id, &[1; 32], &[2; 32], now)
                .await
                .unwrap_err(),
            SdkError::UnknownIdentity(id.to_string())
        );
        assert_eq!(phone.guests().await.unwrap(), []);
        assert_eq!(phone.credential(id).await.unwrap(), None);
        assert_eq!(
            storage
                .secret(&keys::portable_device(&id.to_string()))
                .await
                .unwrap(),
            None
        );
        assert_eq!(phone.issued(id).await.unwrap(), issued);
        assert_eq!(storage.data(&serial).await.unwrap(), serial_before);
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

    /// The serial is the issue time in milliseconds, so two holders of one
    /// root (a synced phrase root) almost never give the same one, and a
    /// later renewal from either is larger; and never less than one past
    /// the last here, so it grows on one holder even when its clock goes
    /// back, across a restart too.
    #[tokio::test]
    async fn the_serial_is_the_time_in_milliseconds_and_always_grows() {
        let (storage, phone) = portable();
        let (id, _) = phone.create("Steve").await.unwrap();
        let key = phone.device_key(id).await.unwrap();
        let millis = |at: i64| u64::try_from(at).unwrap() * 1000;
        let first = phone.issue_credential_at(id, key, None, T).await.unwrap();
        let renewed = phone
            .issue_credential_at(id, key, None, T + 1)
            .await
            .unwrap();
        assert_eq!((first.serial, renewed.serial), (millis(T), millis(T + 1)));
        let restarted = Portable::new(storage);
        assert_eq!(
            restarted
                .issue_credential_at(id, key, None, T)
                .await
                .unwrap()
                .serial,
            millis(T + 1) + 1
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
        // A rival at the same revision: kept, and, the root being here,
        // merged at once into one state at the next revision.
        assert!(carrier.accept_state(from_second.clone()).await.unwrap());
        let merged = carrier.identity_states(id).await.unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].revision, 2);
        assert!(merged[0].revokes(&tv_key) && merged[0].revokes(&laptop_key));
        assert_eq!(
            carrier
                .issue_credential_at(id, laptop_key, None, T)
                .await
                .unwrap_err(),
            SdkError::DeviceKeyRevoked
        );
        // And its next state carries both.
        let next = carrier.revoke_at(id, some_key(9), T + 1).await.unwrap();
        assert_eq!(next.revision, 3);
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
                (tv, renewed.serial, T + 5, Some("Living room")),
                (laptop, T as u64 * 1000 + 1, T, Some("Laptop"))
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

    /// P7-06: the root's phone signs its own key a credential, keeps it
    /// while it holds for long, and renews it near its end.
    #[tokio::test]
    async fn the_root_s_phone_signs_itself_a_credential_and_renews_it_late() {
        let (phone, id) = phone().await;
        let first = phone.own_credential_at(id, T).await.unwrap();
        assert_eq!(first.device_key, phone.device_key(id).await.unwrap());
        assert_eq!(phone.credential(id).await.unwrap(), Some(first.clone()));
        assert_eq!(
            phone.own_credential_at(id, T + 86_400).await.unwrap(),
            first
        );
        let late = first.expires_at - OWN_CREDENTIAL_RENEW + 1;
        let renewed = phone.own_credential_at(id, late).await.unwrap();
        assert!(renewed.serial > first.serial);
        assert_eq!(phone.credential(id).await.unwrap(), Some(renewed));
        let issued = phone.issued(id).await.unwrap();
        assert_eq!(issued.len(), 1);
        assert_eq!(issued[0].label.as_deref(), Some(OWN_DEVICE_LABEL));
    }

    /// A device without the root keeps what it was given.
    #[tokio::test]
    async fn a_device_without_the_root_uses_the_credential_it_was_given() {
        let (phone, id) = phone().await;
        let tv = device_of(&phone, id).await;
        let given = tv.credential(id).await.unwrap().unwrap();
        let late = given.expires_at - OWN_CREDENTIAL_RENEW + 1;
        assert_eq!(tv.own_credential_at(id, late).await.unwrap(), given);
    }

    #[tokio::test]
    async fn a_renamed_identity_is_listed_by_its_new_name() {
        let (_, portable) = portable();
        let (id, _) = portable.create("Steve").await.unwrap();
        portable.rename(id, "  Stephen ").await.unwrap();
        let listed = portable.list().await.unwrap();
        assert_eq!((listed[0].id, listed[0].name.as_str()), (id, "Stephen"));
    }

    #[tokio::test]
    async fn renaming_an_identity_not_here_is_unknown() {
        let (_, portable) = portable();
        let (other, _) = PortableRoot::generate();
        assert_eq!(
            portable.rename(other.id(), "Ann").await.unwrap_err(),
            SdkError::UnknownIdentity(other.id().to_string())
        );
    }

    #[tokio::test]
    async fn an_empty_or_blank_name_is_refused_and_the_old_one_kept() {
        let (_, portable) = portable();
        let (id, _) = portable.create("Steve").await.unwrap();
        for blank in ["", "   ", "\t\n"] {
            assert!(
                matches!(
                    portable.rename(id, blank).await.unwrap_err(),
                    SdkError::InvalidInput(_)
                ),
                "{blank:?}"
            );
        }
        assert_eq!(portable.list().await.unwrap()[0].name, "Steve");
    }

    #[tokio::test]
    async fn a_name_over_the_limit_is_refused() {
        let (_, portable) = portable();
        let (id, _) = portable.create("Steve").await.unwrap();
        let longest = "é".repeat(MAX_IDENTITY_NAME);
        portable.rename(id, &longest).await.unwrap();
        assert!(matches!(
            portable
                .rename(id, &format!("{longest}e"))
                .await
                .unwrap_err(),
            SdkError::InvalidInput(_)
        ));
        assert_eq!(portable.list().await.unwrap()[0].name, longest);
    }

    #[tokio::test]
    async fn renaming_to_the_same_name_changes_nothing() {
        let (_, portable) = portable();
        let (id, _) = portable.create("Steve").await.unwrap();
        portable.rename(id, "Stephen").await.unwrap();
        let before = portable.list().await.unwrap();
        portable.rename(id, " Stephen").await.unwrap();
        assert_eq!(portable.list().await.unwrap(), before);
    }

    #[tokio::test]
    async fn each_rename_here_is_newer_than_the_one_before() {
        let (_, portable) = portable();
        let (id, _) = portable.create("Steve").await.unwrap();
        assert_eq!(portable.list().await.unwrap()[0].name_set_at, 0);
        portable.rename(id, "A").await.unwrap();
        let first = portable.list().await.unwrap()[0].name_set_at;
        portable.rename(id, "B").await.unwrap();
        assert!(portable.list().await.unwrap()[0].name_set_at > first);
    }

    /// A list saved before names had a time still loads, every name at 0.
    #[tokio::test]
    async fn a_list_saved_without_name_times_loads() {
        let (storage, portable) = portable();
        let id = PortableRoot::recover(&zero_phrase()).unwrap().id();
        let old = format!(r#"[{{"id":"{id}","name":"Ann","created_at":1700000000}}]"#);
        storage
            .set_data(keys::PORTABLE_IDENTITIES, Some(old.into_bytes()))
            .await
            .unwrap();
        let listed = portable.list().await.unwrap();
        assert_eq!((listed[0].name.as_str(), listed[0].name_set_at), ("Ann", 0));
    }
}
