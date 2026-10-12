//! Security key roots (P7-08): a FIDO2 key as a portable identity's
//! root, so a Linux desktop (USB) or an iPhone (NFC) acts for an identity
//! with no root secret on the device. The key signs CTAP2 assertions
//! under [`PORTABLE_RP`] over SHA-256 of the bytes a phrase root signs;
//! the protocol crate checks them.
//!
//! A device talks to its key through [`SecurityKey`]: libfido2 over USB
//! on Linux (raw CTAP2), Apple's system security-key API on iPhone, iPad
//! and Mac (WebAuthn form), [`SoftSecurityKey`] in tests (either). The
//! key is needed only for root actions: creating an identity, issuing,
//! renewing and revoking credentials, adding and removing roots. Signing
//! in and playing use the device's own credential, as before.
//!
//! An identity made here with keys is kept as its record: its genesis
//! ([`IdentityGenesis`]) and newest state. Export it
//! ([`Portable::identity_record`]) and import it on another device
//! ([`Portable::import_record`]), or fetch it from any server that knows
//! the identity ([`crate::Client::fetch_identity_record`]), and any one of
//! its keys acts for it there.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub use spoolframe_protocol::portable::soft_key::SoftKey;
pub use spoolframe_protocol::portable::{
    AddedRoot, GenesisError, HANDLE_LEN, IdentityGenesis, IdentityRecord, MAX_ROOTS, PORTABLE_RP,
    RemovedRoot, RootKey, RootRegistration, RootSet, RootSignature, StateChanges, WebAuthnError,
};
use spoolframe_protocol::portable::{root_add_message, root_endorse_message, root_set};

use super::{
    DeviceKey, DevicePublicKey, MAX_CREDENTIAL_VALIDITY, PortableIdentity, PortableIdentityState,
    PortableUserId, RevokedKey, StateError, UserDeviceCredential, is_revoked, load,
    load_credentials, load_issued, load_revoked, load_state, merge, public_key, revocation_lapsed,
    save, save_issued, save_revoked, save_state, unix_now,
};
use crate::error::SdkError;
use crate::servers::now;
use crate::storage::{Storage, StorageError, keys};

/// How long before a credential expires it is due for renewal: its last
/// 14 days. The device shows "Renew soon" with the date.
pub const RENEWAL_DUE: i64 = 14 * 24 * 3600;

/// What a security key answered an assertion with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyAssertion {
    /// The credential that signed: one of those allowed, or the
    /// discoverable one the key chose.
    pub credential_id: Vec<u8>,
    pub signature: RootSignature,
    /// The credential's `user.id` (the identity's handle), which a key
    /// returns when the allow list was empty.
    pub user_handle: Option<[u8; HANDLE_LEN]>,
}

/// Why a security key did not do what was asked (P7-08), as the Linux
/// client's libfido2 transport (`spoolframe_linux_core::fido2`) and the
/// iPhone's Core NFC tell them apart, so the app says the right thing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecurityKeyError {
    #[error("the PIN is wrong")]
    WrongPin,
    /// Too many wrong PINs since it was plugged in: unplug it and plug
    /// it back in to try again.
    #[error("unplug the key and plug it back in")]
    Replug,
    /// Too many wrong PINs in all: the key is locked until reset.
    #[error("the key is locked")]
    Locked,
    /// The key has no PIN set, and a root action needs one.
    #[error("the key has no PIN; set one first")]
    NoPin,
    /// None of the identity's credentials is on this key.
    #[error("this key holds none of the identity's credentials")]
    NoCredential,
    /// No room for another resident credential.
    #[error("the key is full")]
    Full,
    /// Nobody touched it in time.
    #[error("the key was not touched in time")]
    NotTouched,
    /// What it answered does not check out: another site's assertion, or
    /// one made without the touch or the PIN; a WebAuthn assertion of the
    /// wrong type or challenge; a registration made for another identity
    /// or with a key no root can be.
    #[error("the key's signature does not check out")]
    BadAssertion,
    /// Anything else, in the transport's words.
    #[error("the security key: {0}")]
    Other(String),
    /// The key, or the platform between, did not do it, and says no more
    /// than this (and perhaps a message for the log). Apple's system API
    /// shows its own PIN screens, and a wrong, locked or missing PIN all
    /// come back this way.
    #[error("the security key failed{}", .0.as_deref().map(|m| format!(": {m}")).unwrap_or_default())]
    Failed(Option<String>),
    /// The person cancelled: the app says nothing.
    #[error("the security key action was cancelled")]
    Cancelled,
}

/// A FIDO2 security key, as a device reaches it (P7-08): raw CTAP2, as
/// libfido2's `make_credential` and `get_assertion` (the Linux client's
/// `spoolframe_linux_core::fido2`), or a platform's WebAuthn API (Apple's
/// `ASAuthorizationSecurityKeyPublicKeyCredentialProvider`). Object-safe
/// and `Send + Sync`, so an app may implement it across FFI (a uniffi
/// foreign trait wrapped by an adapter); each call may wait on a person.
///
/// Both calls need the person's touch and the key's PIN. **The
/// implementation collects the PIN** (and may keep it for the length of
/// one root action, which asks the key more than once), or leaves it to
/// the platform: the SDK never sees it. User verification comes from the
/// PIN, so there is no "uv" option to pass over raw CTAP2 (the user's
/// Feitian key refuses one); over WebAuthn, ask for
/// `userVerification = required`. The key does not keep a user name; the
/// handle identifies the identity.
///
/// Register with [`Self::register_root`] (either form) or
/// [`Self::register`] (raw CTAP2's root as read); each has a default that
/// uses the other, so implement one. Assert with [`Self::assert`], in
/// either form.
#[async_trait]
pub trait SecurityKey: Send + Sync {
    /// `authenticatorMakeCredential`: a resident (discoverable) ES256
    /// credential under `rp`, for user id `handle`, shown as `name`, with
    /// credProtect 2 (so it is not listed without the PIN). The root it
    /// makes: its public key and credential id. Its client data hash is
    /// `handle`; the attestation is not checked.
    ///
    /// The default is for a key that implements only
    /// [`Self::register_root`]: [`SecurityKeyError::Failed`].
    async fn register(
        &self,
        rp: &str,
        handle: [u8; HANDLE_LEN],
        name: &str,
    ) -> Result<RootKey, SdkError> {
        let _ = (rp, handle, name);
        Err(SecurityKeyError::Failed(Some(
            "this security key registers through register_root".into(),
        ))
        .into())
    }

    /// Make a credential, as [`Self::register`] does, and answer in
    /// whichever form the key's API gives it; the SDK reads the root out
    /// ([`RootRegistration::root`]). This is what the SDK calls.
    ///
    /// Over WebAuthn: `rp.id` = `rp`, `challenge` = `handle` (the 32
    /// bytes), `user.id` = `handle`, `user.name` = `name`, ES256 (-7),
    /// a resident key required, `userVerification` required; answer
    /// [`RootRegistration::WebAuthn`] with the `attestationObject` and the
    /// exact `rawClientDataJSON`. Attestation `none` is enough. That the
    /// credential is resident with `user.id` = `handle` cannot be checked
    /// from the answer: ask for it.
    ///
    /// The default asks [`Self::register`]: [`RootRegistration::Ctap`].
    async fn register_root(
        &self,
        rp: &str,
        handle: [u8; HANDLE_LEN],
        name: &str,
    ) -> Result<RootRegistration, SdkError> {
        self.register(rp, handle, name)
            .await
            .map(RootRegistration::Ctap)
    }

    /// `authenticatorGetAssertion` under `rp` over `client_data_hash`,
    /// with the credentials in `allowed` (ids), or every discoverable one
    /// the key holds for `rp` when `allowed` is empty: all it answers
    /// with (one per identity on the key), each with its user handle.
    /// None is [`SecurityKeyError::NoCredential`].
    ///
    /// `client_data_hash` is SHA-256 of what the root signs. Over raw
    /// CTAP2 it is the client data hash, and the answer is
    /// [`RootSignature::Ctap`]. Over WebAuthn, which writes its own client
    /// data, it is the `challenge` (the 32 bytes as they are, which the
    /// platform base64url-encodes), and the answer is
    /// [`RootSignature::WebAuthn`] with the `authenticatorData`, the exact
    /// `rawClientDataJSON` and the DER signature; `userHandle` is the
    /// handle.
    async fn assert(
        &self,
        rp: &str,
        client_data_hash: [u8; 32],
        allowed: &[Vec<u8>],
    ) -> Result<Vec<KeyAssertion>, SdkError>;
}

/// A security key in software (P7-08): [`SoftKey`]s made by
/// [`SecurityKey::register`], answering as the user's Feitian key does
/// (32-byte credential ids, flags UP and UV, counter 0). For tests, and
/// for trying key roots with no key; its secrets are in memory only.
///
/// It answers in raw CTAP2 form ([`Self::new`]), as libfido2 does, or in
/// WebAuthn form ([`Self::webauthn`]), as Apple's system API does.
/// [`Self::as_webauthn`] and [`Self::as_ctap`] are the same credentials
/// reached the other way: one key on a Linux desktop and on an iPhone.
#[derive(Debug, Default, Clone)]
pub struct SoftSecurityKey {
    credentials: Arc<Mutex<Vec<SoftKey>>>,
    webauthn: bool,
}

impl SoftSecurityKey {
    /// A key that answers in raw CTAP2 form.
    pub fn new() -> Self {
        Self::default()
    }

    /// A key that answers in WebAuthn form.
    pub fn webauthn() -> Self {
        Self::new().as_webauthn()
    }

    /// The same credentials, answering in WebAuthn form.
    pub fn as_webauthn(&self) -> Self {
        Self {
            credentials: self.credentials.clone(),
            webauthn: true,
        }
    }

    /// The same credentials, answering in raw CTAP2 form.
    pub fn as_ctap(&self) -> Self {
        Self {
            credentials: self.credentials.clone(),
            webauthn: false,
        }
    }

    /// A new credential for `handle` under `rp`, kept.
    fn make(&self, rp: &str, handle: [u8; HANDLE_LEN]) -> Result<SoftKey, SdkError> {
        if rp != PORTABLE_RP {
            return Err(SecurityKeyError::Other(format!("no credentials under {rp}")).into());
        }
        let key = loop {
            let mut random = Zeroizing::new([0u8; 64]);
            getrandom::fill(random.as_mut()).expect("the OS gives random bytes");
            if let Some(key) = SoftKey::from_random(*random) {
                break key;
            }
        }
        .with_handle(handle);
        self.credentials.lock().push(key.clone());
        Ok(key)
    }

    /// The credentials it holds, newest last.
    pub fn credentials(&self) -> Vec<SoftKey> {
        self.credentials.lock().clone()
    }
}

#[async_trait]
impl SecurityKey for SoftSecurityKey {
    async fn register(
        &self,
        rp: &str,
        handle: [u8; HANDLE_LEN],
        _name: &str,
    ) -> Result<RootKey, SdkError> {
        Ok(self.make(rp, handle)?.root())
    }

    async fn register_root(
        &self,
        rp: &str,
        handle: [u8; HANDLE_LEN],
        _name: &str,
    ) -> Result<RootRegistration, SdkError> {
        let key = self.make(rp, handle)?;
        Ok(if self.webauthn {
            key.register_webauthn(&handle)
        } else {
            key.register_ctap()
        })
    }

    async fn assert(
        &self,
        rp: &str,
        client_data_hash: [u8; 32],
        allowed: &[Vec<u8>],
    ) -> Result<Vec<KeyAssertion>, SdkError> {
        let answers: Vec<KeyAssertion> = self
            .credentials
            .lock()
            .iter()
            .filter(|k| {
                rp == PORTABLE_RP
                    && (allowed.is_empty() || allowed.iter().any(|id| id == k.credential_id()))
            })
            .map(|key| KeyAssertion {
                credential_id: key.credential_id().to_vec(),
                signature: if self.webauthn {
                    key.assert_webauthn_prehashed(client_data_hash)
                } else {
                    key.assert_prehashed(client_data_hash)
                },
                user_handle: key.user_handle(),
            })
            .collect();
        if answers.is_empty() {
            return Err(SecurityKeyError::NoCredential.into());
        }
        Ok(answers)
    }
}

/// Who signs a root action (P7-08): the phrase root this device holds, or
/// a security key that is one of the identity's roots.
#[derive(Clone, Copy)]
pub enum RootSigner<'a> {
    Phrase,
    Key(&'a dyn SecurityKey),
}

impl std::fmt::Debug for RootSigner<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RootSigner::Phrase => f.write_str("RootSigner::Phrase"),
            RootSigner::Key(_) => f.write_str("RootSigner::Key"),
        }
    }
}

/// A root's credential id, if it is a security key's.
fn credential_id(root: &RootKey) -> Option<Vec<u8>> {
    match root {
        RootKey::SecurityKey { credential_id, .. } => Some(credential_id.clone()),
        RootKey::Phrase(_) => None,
    }
}

/// What a security key's root signs, as its `clientDataHash`.
fn client_data_hash(message: &[u8]) -> [u8; 32] {
    Sha256::digest(message).into()
}

/// Have `key` make a credential for the identity `handle`, and read its
/// root: in either form, checked here (a WebAuthn registration for another
/// identity, or of a key no root can be, is
/// [`SecurityKeyError::BadAssertion`]).
async fn register_with(
    key: &dyn SecurityKey,
    handle: [u8; HANDLE_LEN],
    name: &str,
) -> Result<RootKey, SdkError> {
    let registration = key.register_root(PORTABLE_RP, handle, name).await?;
    registration.root(&handle).map_err(|e| {
        tracing::warn!(error = %e, "a security key's registration does not check out");
        SecurityKeyError::BadAssertion.into()
    })
}

/// Have `key` assert over `message` with one of `roots` (credential ids),
/// and say which root signed. The assertion is checked here, so a key
/// that answered for another RP or without the PIN fails now, not on a
/// server. A key that holds none of `roots` (another identity's, a
/// removed one, or none at all) is [`SdkError::NotARoot`]: "this key
/// isn't one of the identity's", whatever the transport called it.
async fn assert_with(
    key: &dyn SecurityKey,
    roots: &[RootKey],
    message: &[u8],
) -> Result<(RootKey, RootSignature), SdkError> {
    let allowed: Vec<Vec<u8>> = roots.iter().filter_map(credential_id).collect();
    if allowed.is_empty() {
        return Err(SdkError::NotARoot);
    }
    let answers = match key
        .assert(PORTABLE_RP, client_data_hash(message), &allowed)
        .await
    {
        Ok(answers) => answers,
        Err(SdkError::SecurityKey(SecurityKeyError::NoCredential)) => {
            return Err(SdkError::NotARoot);
        }
        Err(e) => return Err(e),
    };
    // The first answer from one of `roots`: a key holds one credential per
    // identity, and this asked for this identity's only.
    for answer in answers {
        let Some(root) = roots
            .iter()
            .find(|r| credential_id(r).as_deref() == Some(answer.credential_id.as_slice()))
        else {
            continue;
        };
        if !root.verify(message, &answer.signature) {
            return Err(SecurityKeyError::BadAssertion.into());
        }
        return Ok((root.clone(), answer.signature));
    }
    Err(SdkError::NotARoot)
}

impl super::Portable {
    /// A new identity called `name`, rooted in `keys` (P7-08), one or more
    /// up to [`MAX_ROOTS`]: the first is the one in hand, the others its
    /// backups (one in a drawer). Each is asked twice, to make its
    /// credential and to sign the genesis: a touch and the PIN each time.
    /// No secret is kept here: the identity is its genesis, kept with its
    /// id ([`Self::identity_record`]). Its id is derived from the genesis.
    pub async fn create_with_keys(
        &self,
        keys: &[&dyn SecurityKey],
        name: &str,
    ) -> Result<PortableUserId, SdkError> {
        if keys.is_empty() || keys.len() > MAX_ROOTS {
            return Err(SdkError::InvalidInput(format!(
                "an identity has 1 to {MAX_ROOTS} roots"
            )));
        }
        let mut handle = [0u8; HANDLE_LEN];
        getrandom::fill(&mut handle).expect("the OS gives random bytes");
        let mut roots = Vec::with_capacity(keys.len());
        for key in keys {
            let root = register_with(*key, handle, name).await?;
            if !root.is_valid() || roots.contains(&root) {
                return Err(SecurityKeyError::Other(
                    "it made a credential that is not a P-256 key, or one given already".into(),
                )
                .into());
            }
            roots.push(root);
        }
        let message = IdentityGenesis::message(&handle, &roots).expect("at most 8 roots");
        let mut signatures = Vec::with_capacity(keys.len());
        for (key, root) in keys.iter().zip(&roots) {
            let (_, signature) = assert_with(*key, std::slice::from_ref(root), &message).await?;
            signatures.push(signature);
        }
        let genesis = IdentityGenesis {
            handle,
            roots,
            signatures,
        };
        genesis
            .verify()
            .map_err(|e| SdkError::InvalidInput(e.to_string()))?;
        let id = genesis.user();
        self.keep_record(&genesis, None, name).await?;
        tracing::info!(%id, roots = genesis.roots.len(), "made a portable identity rooted in security keys");
        Ok(id)
    }

    /// What this device keeps of identity `id` to check it and hand on
    /// (P7-08): its genesis (none for a phrase identity) and newest
    /// state. Export it to another device, or keep it with a backup key:
    /// with any one root, it is the identity.
    pub async fn identity_record(&self, id: PortableUserId) -> Result<IdentityRecord, SdkError> {
        let storage = self.storage.as_ref();
        Ok(IdentityRecord {
            user: id,
            genesis: load_genesis(storage, id).await?,
            state: load_state(storage, id).await?,
        })
    }

    /// Keep `record` on this device as identity `name` (P7-08): from
    /// another device, an export file, or a server that knows the
    /// identity. Checked first: a genesis must be the user's, a state
    /// signed by one of its roots. Then any of its keys acts for it here.
    /// An identity already here keeps its name, and takes the record's
    /// state if it is newer.
    pub async fn import_record(
        &self,
        record: IdentityRecord,
        name: &str,
    ) -> Result<PortableUserId, SdkError> {
        let id = record.user;
        if let Some(genesis) = &record.genesis {
            genesis
                .verify()
                .map_err(|e| SdkError::InvalidInput(e.to_string()))?;
            if genesis.user() != id {
                return Err(SdkError::InvalidInput(
                    "the genesis is another identity's".into(),
                ));
            }
        } else if !RootKey::phrase_of(&id).is_valid() {
            return Err(SdkError::InvalidInput(
                "an identity with security key roots needs its genesis".into(),
            ));
        }
        if let Some(state) = &record.state {
            state.verify()?;
            if state.user != id {
                return Err(SdkError::InvalidInput(
                    "the record's state is another identity's".into(),
                ));
            }
            let roots = root_set(&id, record.genesis.as_ref(), std::slice::from_ref(state));
            if !roots.knew(&state.signer()) {
                // Data from elsewhere, not a key in hand: not NotARoot.
                return Err(StateError::BadSignature.into());
            }
        }
        let Some(genesis) = record.genesis else {
            // A phrase identity: its phrase recovers it; a record only
            // carries its state.
            if let Some(state) = record.state {
                self.keep_entry(id, name, false).await?;
                self.accept_state(state).await?;
            }
            return Ok(id);
        };
        self.keep_record(&genesis, record.state.as_ref(), name)
            .await?;
        tracing::info!(%id, "imported a portable identity's record");
        Ok(id)
    }

    /// The roots of `id` as this device knows them (P7-08): from its
    /// genesis, or the phrase root its id is, and the newest state here.
    pub async fn roots(&self, id: PortableUserId) -> Result<RootSet, SdkError> {
        let storage = self.storage.as_ref();
        let genesis = load_genesis(storage, id).await?;
        let states: Vec<PortableIdentityState> =
            load_state(storage, id).await?.into_iter().collect();
        Ok(root_set(&id, genesis.as_ref(), &states))
    }

    /// Sign `message` for `id` with `signer`: the phrase root here, or a
    /// key that is one of `id`'s roots now.
    async fn sign_root(
        &self,
        id: PortableUserId,
        signer: RootSigner<'_>,
        message: &[u8],
    ) -> Result<(RootKey, RootSignature), SdkError> {
        let roots = self.roots(id).await?;
        match signer {
            RootSigner::Phrase => {
                let root = self.load_root(id).await?;
                let key = RootKey::phrase_of(&id);
                if !roots.holds(&key) {
                    return Err(SdkError::NotARoot);
                }
                Ok((key, root.sign(message).into()))
            }
            RootSigner::Key(key) => assert_with(key, &roots.members, message).await,
        }
    }

    /// A credential for `device_key` from `signer`, one of `id`'s roots
    /// (P7-08): as [`Self::issue_credential`], with a security key or the
    /// phrase root. A key identity's credential carries its genesis, for
    /// servers that have not seen it.
    pub async fn issue_credential_by(
        &self,
        id: PortableUserId,
        signer: RootSigner<'_>,
        device_key: DevicePublicKey,
        label: Option<&str>,
    ) -> Result<UserDeviceCredential, SdkError> {
        self.issue_credential_by_at(id, signer, device_key, label, unix_now())
            .await
    }

    pub(crate) async fn issue_credential_by_at(
        &self,
        id: PortableUserId,
        signer: RootSigner<'_>,
        device_key: DevicePublicKey,
        label: Option<&str>,
        now: i64,
    ) -> Result<UserDeviceCredential, SdkError> {
        let storage = self.storage.as_ref();
        if is_revoked(storage, id, &device_key).await? {
            return Err(SdkError::DeviceKeyRevoked);
        }
        let serial = {
            let _saving = self.saving.lock().await;
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
            serial
        };
        let expires_at = now.saturating_add(MAX_CREDENTIAL_VALIDITY);
        let message = UserDeviceCredential::message(&id, &device_key, serial, now, expires_at);
        // The key is asked outside the lock: it waits on a touch and a PIN.
        let (root, signature) = self.sign_root(id, signer, &message).await?;
        let mut credential =
            UserDeviceCredential::signed_by(id, root, device_key, serial, now, expires_at, |_| {
                signature
            });
        credential.genesis = load_genesis(storage, id).await?.map(Box::new);
        let _saving = self.saving.lock().await;
        let mut issued = load_issued(storage, id).await?;
        let record = super::IssuedCredential {
            device_key,
            serial,
            issued_at: now,
            expires_at,
            label: None,
            revoked_at: None,
        };
        match issued.iter_mut().find(|e| e.device_key == device_key) {
            Some(entry) => {
                let label = label.map(str::to_owned).or(entry.label.take());
                *entry = super::IssuedCredential { label, ..record };
            }
            None => issued.push(super::IssuedCredential {
                label: label.map(str::to_owned),
                ..record
            }),
        }
        save_issued(storage, id, &issued).await?;
        tracing::info!(%id, %device_key, serial, "issued a portable device credential");
        Ok(credential)
    }

    /// A new state for `id` signed by `signer` (P7-08), one revision past
    /// the newest here, with every live revocation and root change kept
    /// and `change` applied. Kept here as the newest; carry it to servers.
    pub(super) async fn new_state(
        &self,
        id: PortableUserId,
        signer: RootSigner<'_>,
        known: Option<RootKey>,
        change: impl FnOnce(&mut StateChanges),
        now: i64,
    ) -> Result<PortableIdentityState, SdkError> {
        let storage = self.storage.as_ref();
        let previous = load_state(storage, id).await?;
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
        let mut changes = StateChanges {
            revoked: union
                .iter()
                .filter(|&(_, &at)| !revocation_lapsed(at, now))
                .map(|(&key, &revoked_at)| RevokedKey { key, revoked_at })
                .collect(),
            roots_added: previous
                .as_ref()
                .map(|p| p.roots_added.clone())
                .unwrap_or_default(),
            roots_removed: previous
                .as_ref()
                .map(|p| p.roots_removed.clone())
                .unwrap_or_default(),
        };
        change(&mut changes);
        let roots = self.roots(id).await?;
        let signer_root = match signer {
            RootSigner::Phrase => RootKey::phrase_of(&id),
            RootSigner::Key(key) => {
                return self
                    .new_state_by_key(id, key, known, &roots, revision, changes, now)
                    .await;
            }
        };
        if !roots.holds(&signer_root) {
            return Err(SdkError::NotARoot);
        }
        let root = self.load_root(id).await?;
        let state =
            PortableIdentityState::signed_by(id, signer_root, revision, changes, now, |m| {
                root.sign(m).into()
            })?;
        self.keep_state(&state, &union).await?;
        Ok(state)
    }

    /// [`Self::new_state`] signed by a security key. The bytes name the
    /// signer, so unless it is `known` (from an endorsement it just made)
    /// the key is first asked which of the identity's roots it holds (an
    /// assertion over a probe), then to sign the state.
    #[allow(clippy::too_many_arguments)]
    async fn new_state_by_key(
        &self,
        id: PortableUserId,
        key: &dyn SecurityKey,
        known: Option<RootKey>,
        roots: &RootSet,
        revision: u64,
        changes: StateChanges,
        now: i64,
    ) -> Result<PortableIdentityState, SdkError> {
        let signer = match known {
            Some(root) => root,
            None => self.which_root(id, key, roots).await?,
        };
        let draft = PortableIdentityState::signed_by(
            id,
            signer.clone(),
            revision,
            changes.clone(),
            now,
            |_| RootSignature::Ed25519(super::Signature::from_bytes(&[0; 64])),
        )?;
        let (_, signature) =
            assert_with(key, std::slice::from_ref(&signer), &draft.signed_message()).await?;
        let state =
            PortableIdentityState::signed_by(id, signer, revision, changes, now, |_| signature)?;
        state.verify()?;
        let union = load_revoked(self.storage.as_ref(), id).await?;
        self.keep_state(&state, &union).await?;
        Ok(state)
    }

    /// Which of `id`'s roots `key` holds: it is asked to assert over a
    /// probe naming the identity, with every member's credential id.
    async fn which_root(
        &self,
        id: PortableUserId,
        key: &dyn SecurityKey,
        roots: &RootSet,
    ) -> Result<RootKey, SdkError> {
        let mut probe = b"spoolframe which root v1\0".to_vec();
        probe.extend_from_slice(id.as_bytes());
        let (root, _) = assert_with(key, &roots.members, &probe).await?;
        Ok(root)
    }

    /// Keep `state` as the newest here, and `union` with its revocations.
    async fn keep_state(
        &self,
        state: &PortableIdentityState,
        union: &super::Revoked,
    ) -> Result<(), SdkError> {
        let storage = self.storage.as_ref();
        let _saving = self.saving.lock().await;
        let mut union = union.clone();
        merge(&mut union, &state.revoked);
        save_revoked(storage, state.user, &union).await?;
        save_state(storage, state).await?;
        Ok(())
    }

    /// Revoke `device_key` for `id`, signed by `signer` (P7-08): as
    /// [`Self::revoke`], with a security key or the phrase root.
    pub async fn revoke_by(
        &self,
        id: PortableUserId,
        signer: RootSigner<'_>,
        device_key: DevicePublicKey,
    ) -> Result<PortableIdentityState, SdkError> {
        let now = unix_now();
        let state = self
            .new_state(
                id,
                signer,
                None,
                |changes| {
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

    /// Add `new_key` as a root of `id` (P7-08), signed by `signer`, one of
    /// its roots now: `new_key` makes a credential for the identity's
    /// handle and proves it holds it, then the state lists it. A phrase
    /// identity may add a key, and keeps its id. Carry the state to
    /// servers so they take credentials the new key signs.
    pub async fn add_root(
        &self,
        id: PortableUserId,
        signer: RootSigner<'_>,
        new_key: &dyn SecurityKey,
        name: &str,
    ) -> Result<PortableIdentityState, SdkError> {
        let handle = load_genesis(self.storage.as_ref(), id)
            .await?
            .map_or(*id.as_bytes(), |g| g.handle);
        // Checked before the key is asked: each registration takes one of
        // its few resident slots, and a refused one would leave it there.
        let roots = self.roots(id).await?;
        if roots.members.len() >= MAX_ROOTS {
            return Err(SdkError::InvalidInput(format!(
                "an identity has at most {MAX_ROOTS} roots"
            )));
        }
        let root = register_with(new_key, handle, name).await?;
        // A credential no root can carry (an id over 255 bytes) would
        // panic in `RootKey::bytes`; refused as `create_with_keys` does.
        if !root.is_valid() {
            return Err(SecurityKeyError::Other(
                "it made a credential that is not a P-256 key".into(),
            )
            .into());
        }
        // Only known once the key made it: a key gives a new credential
        // each time, so this is a key that answers with one it gave before.
        if roots.knew(&root) {
            return Err(SdkError::InvalidInput(
                "that key is, or was, one of the identity's roots".into(),
            ));
        }
        let (_, possession) = assert_with(
            new_key,
            std::slice::from_ref(&root),
            &root_add_message(&id, &root),
        )
        .await?;
        let now = unix_now();
        let (added_by, endorsement) = self
            .sign_root(id, signer, &root_endorse_message(&id, &root, now))
            .await?;
        let added = AddedRoot {
            root,
            added_by: added_by.clone(),
            added_at: now,
            endorsement,
            possession,
        };
        let state = self
            .new_state(
                id,
                signer,
                Some(added_by),
                |changes| changes.roots_added.push(added),
                now,
            )
            .await?;
        tracing::info!(%id, revision = state.revision, "added a root to a portable identity");
        Ok(state)
    }

    /// Remove `root` from `id`'s roots (P7-08), signed by `signer`. It
    /// never comes back, and whatever it signs from then on is refused
    /// wherever the state reaches. Removing the last root locks the
    /// identity: refused here.
    pub async fn remove_root(
        &self,
        id: PortableUserId,
        signer: RootSigner<'_>,
        root: &RootKey,
    ) -> Result<PortableIdentityState, SdkError> {
        let roots = self.roots(id).await?;
        if !roots.holds(root) {
            return Err(SdkError::NotARoot);
        }
        if roots.members.len() == 1 {
            return Err(SdkError::InvalidInput(
                "that is the identity's last root: removing it locks the identity".into(),
            ));
        }
        let now = unix_now();
        let removed = RemovedRoot {
            root: root.clone(),
            removed_at: now,
        };
        let state = self
            .new_state(
                id,
                signer,
                None,
                |changes| changes.roots_removed.push(removed),
                now,
            )
            .await?;
        tracing::info!(%id, revision = state.revision, "removed a root of a portable identity");
        Ok(state)
    }

    /// Pair this device itself with `id` (P7-08): the device that holds
    /// the key is both the device and the root terminal. Its key gets a
    /// credential from `signer` and installs it. Also how it renews its
    /// own credential, with one touch.
    pub async fn pair_self(
        &self,
        id: PortableUserId,
        signer: RootSigner<'_>,
    ) -> Result<UserDeviceCredential, SdkError> {
        let key = self.device_key(id).await?;
        let credential = self
            .issue_credential_by(id, signer, key, Some(super::OWN_DEVICE_LABEL))
            .await?;
        self.install_credential(credential.clone()).await?;
        Ok(credential)
    }

    /// When this device's credential for `id` expires, if it is due for
    /// renewal: in its last [`RENEWAL_DUE`] (14 days), or past. `None`
    /// when it is not due, or there is none. Show "Renew soon" with the
    /// date; [`Self::pair_self`] renews on the device with the key, and
    /// any other device renews by pairing again (same device key).
    pub async fn renewal_due(&self, id: PortableUserId) -> Result<Option<i64>, SdkError> {
        self.renewal_due_at(id, unix_now()).await
    }

    pub(crate) async fn renewal_due_at(
        &self,
        id: PortableUserId,
        now: i64,
    ) -> Result<Option<i64>, SdkError> {
        Ok(load_credentials(self.storage.as_ref())
            .await?
            .remove(&id)
            .map(|c| c.expires_at)
            .filter(|&expires_at| expires_at - now <= RENEWAL_DUE))
    }

    /// Install `credential` with `state`, the identity's newest as the
    /// root terminal sent it (a pairing, P7-03 or P7-08). The state is
    /// checked and kept first, so a credential a root it adds signed (a
    /// phrase identity's new key) checks out; should the credential then
    /// be refused, the state this device had before is put back. Its
    /// revocations join the ones here only once the credential is in.
    ///
    /// A genesis the credential carries must hold and be `id`'s, else
    /// [`PairingError::CredentialRefused`]: its roots decide which states
    /// are believed, so an unchecked one would let anyone's phone keep a
    /// state of its own making for a guest already here.
    ///
    /// [`PairingError::CredentialRefused`]: crate::pairing::PairingError::CredentialRefused
    pub(crate) async fn install_with_state(
        &self,
        credential: UserDeviceCredential,
        state: Option<PortableIdentityState>,
    ) -> Result<(), SdkError> {
        self.install_with_state_for(credential, state, DeviceKey::Kept)
            .await
    }

    /// [`Self::install_with_state`] for the device key `key` names: the
    /// pending one, for a pairing through a server, is adopted only if
    /// the credential is installed.
    pub(crate) async fn install_with_state_for(
        &self,
        credential: UserDeviceCredential,
        state: Option<PortableIdentityState>,
        key: DeviceKey,
    ) -> Result<(), SdkError> {
        let id = credential.user;
        if let Some(genesis) = credential.genesis.as_deref()
            && (genesis.verify().is_err() || genesis.user() != id)
        {
            return Err(crate::pairing::PairingError::CredentialRefused.into());
        }
        let storage = self.storage.as_ref();
        let had = load_state(storage, id).await?;
        let mut stored = None;
        if let Some(state) = &state {
            state.verify()?;
            if state.user != id {
                return Err(SdkError::UnknownIdentity(state.user.to_string()));
            }
            let kept = load_genesis(storage, id).await?;
            let genesis = credential.genesis.as_deref().or(kept.as_ref());
            if !root_set(&id, genesis, std::slice::from_ref(state)).knew(&state.signer())
                || self.roots(id).await?.refuses_additions(&state.roots_added)
            {
                // What the root terminal sent, not a key in hand.
                return Err(StateError::BadSignature.into());
            }
            if state.revokes(&credential.device_key) {
                return Err(SdkError::DeviceKeyRevoked);
            }
            if had.as_ref().is_none_or(|h| h.revision < state.revision) {
                // The state alone for now: the credential's check reads
                // it, and the revocations wait for the credential.
                let _saving = self.saving.lock().await;
                save_state(storage, state).await?;
                stored = Some(state);
            }
        }
        if let Err(e) = self
            .install_credential_for(credential, unix_now(), key)
            .await
        {
            if stored.is_some() {
                let _saving = self.saving.lock().await;
                match &had {
                    Some(had) => save_state(storage, had).await?,
                    None => {
                        storage
                            .set_data(&keys::portable_state(&id.to_string()), None)
                            .await?
                    }
                }
            }
            return Err(e);
        }
        if let Some(state) = stored {
            let _saving = self.saving.lock().await;
            let mut union = load_revoked(storage, id).await?;
            if merge(&mut union, &state.revoked) {
                save_revoked(storage, id, &union).await?;
            }
        }
        Ok(())
    }

    /// A device key for a pairing through a server (P7-08), made before
    /// the identity is known: the same one until it is adopted.
    pub(crate) async fn pending_device_key(&self) -> Result<DevicePublicKey, SdkError> {
        let _saving = self.saving.lock().await;
        let storage = self.storage.as_ref();
        if let Some(key) = load_pending(storage).await? {
            return Ok(public_key(&key));
        }
        let mut secret = Zeroizing::new([0u8; 32]);
        getrandom::fill(secret.as_mut()).expect("the OS gives random bytes");
        storage
            .set_secret(keys::PORTABLE_PENDING_DEVICE, Some(secret.to_vec()))
            .await?;
        Ok(public_key(&ed25519_dalek::SigningKey::from_bytes(&secret)))
    }

    /// Keep `genesis` (and `state`) for its identity, listed as `name`.
    async fn keep_record(
        &self,
        genesis: &IdentityGenesis,
        state: Option<&PortableIdentityState>,
        name: &str,
    ) -> Result<(), SdkError> {
        let id = genesis.user();
        let storage = self.storage.as_ref();
        {
            let _saving = self.saving.lock().await;
            storage
                .set_data(
                    &keys::portable_genesis(&id.to_string()),
                    Some(serde_json::to_vec(genesis).expect("a genesis serializes")),
                )
                .await?;
        }
        self.keep_entry(id, name, true).await?;
        if let Some(state) = state {
            self.accept_state(state.clone()).await?;
        }
        Ok(())
    }

    /// List `id` as `name`, if it is not listed yet.
    async fn keep_entry(
        &self,
        id: PortableUserId,
        name: &str,
        key_rooted: bool,
    ) -> Result<(), SdkError> {
        let storage = self.storage.as_ref();
        let _saving = self.saving.lock().await;
        let mut saved = load(storage).await?;
        if saved.iter().all(|entry| entry.id != id) {
            saved.push(PortableIdentity {
                id,
                name: name.to_owned(),
                created_at: now(),
                has_root: false,
                key_rooted,
            });
            save(storage, &saved).await?;
        }
        Ok(())
    }
}

/// The genesis kept for `id`, if any; unreadable is an error.
pub(super) async fn load_genesis(
    storage: &dyn Storage,
    id: PortableUserId,
) -> Result<Option<IdentityGenesis>, StorageError> {
    let Some(bytes) = storage
        .data(&keys::portable_genesis(&id.to_string()))
        .await?
    else {
        return Ok(None);
    };
    let genesis: IdentityGenesis = serde_json::from_slice(&bytes)
        .map_err(|e| StorageError(format!("the genesis of {id} cannot be read: {e}")))?;
    if genesis.user() != id {
        return Err(StorageError(format!(
            "the genesis stored for {id} is another identity's"
        )));
    }
    Ok(Some(genesis))
}

pub(super) async fn load_pending(
    storage: &dyn Storage,
) -> Result<Option<ed25519_dalek::SigningKey>, StorageError> {
    let Some(bytes) = storage.secret(keys::PORTABLE_PENDING_DEVICE).await? else {
        return Ok(None);
    };
    let bytes = Zeroizing::new(bytes);
    let secret: &[u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| StorageError("the pending device key is not 32 bytes".into()))?;
    Ok(Some(ed25519_dalek::SigningKey::from_bytes(secret)))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::portable::Portable;
    use crate::storage::MemoryStorage;

    fn portable() -> Portable {
        Portable::new(Arc::new(MemoryStorage::default()))
    }

    /// An identity on a desktop, rooted in keys `a` (in hand) and `b`
    /// (the backup).
    async fn key_identity() -> (Portable, PortableUserId, SoftSecurityKey, SoftSecurityKey) {
        let (desktop, a, b) = (portable(), SoftSecurityKey::new(), SoftSecurityKey::new());
        let id = desktop.create_with_keys(&[&a, &b], "Sam").await.unwrap();
        (desktop, id, a, b)
    }

    /// A phrase identity that added key `k` and then removed its phrase
    /// root, on the phone that still holds the phrase's secret.
    async fn phrase_removed() -> (Portable, PortableUserId, SoftSecurityKey) {
        let phone = portable();
        let (id, _phrase) = phone.create("Sam").await.unwrap();
        let k = SoftSecurityKey::new();
        phone
            .add_root(id, RootSigner::Phrase, &k, "K")
            .await
            .unwrap();
        phone
            .remove_root(id, RootSigner::Key(&k), &RootKey::phrase_of(&id))
            .await
            .unwrap();
        (phone, id, k)
    }

    /// P7: a removed phrase root gives no more credentials.
    #[tokio::test]
    async fn a_removed_phrase_root_issues_no_credential() {
        let (phone, id, _k) = phrase_removed().await;
        let tv_key = portable().device_key(id).await.unwrap();
        let issued = phone.issue_credential(id, tv_key, None).await;
        assert!(matches!(issued, Err(SdkError::NotARoot)), "{issued:?}");
    }

    /// P7: a removed phrase root signs no tie; the phone ties with its
    /// device credential instead.
    #[tokio::test]
    async fn a_removed_phrase_root_ties_with_the_device_credential() {
        let (phone, id, k) = phrase_removed().await;
        let credential = phone.pair_self(id, RootSigner::Key(&k)).await.unwrap();
        let proof = phone
            .tie_proof(id, &[0x5a; 32], &[0xa5; 32], "sam")
            .await
            .unwrap();
        match proof {
            spoolframe_protocol::portable::TieProof::Device {
                credential: sent, ..
            } => assert_eq!(sent, credential),
            other => panic!("tied with {other:?}"),
        }
    }

    /// A key whose credentials have ids longer than a root may carry.
    struct LongIdKey;

    #[async_trait]
    impl SecurityKey for LongIdKey {
        async fn register(
            &self,
            rp: &str,
            handle: [u8; HANDLE_LEN],
            name: &str,
        ) -> Result<RootKey, SdkError> {
            match SoftSecurityKey::new().register(rp, handle, name).await? {
                RootKey::SecurityKey { public_key, .. } => Ok(RootKey::SecurityKey {
                    public_key,
                    credential_id: vec![7; 300],
                }),
                phrase => Ok(phrase),
            }
        }

        async fn assert(
            &self,
            _: &str,
            _: [u8; 32],
            _: &[Vec<u8>],
        ) -> Result<Vec<KeyAssertion>, SdkError> {
            Err(SecurityKeyError::NoCredential.into())
        }
    }

    /// P6: a key that makes a credential no root can carry is refused, not
    /// a panic.
    #[tokio::test]
    async fn adding_a_key_with_an_overlong_credential_id_is_refused() {
        let (desktop, id, a, _) = key_identity().await;
        let added = desktop
            .add_root(id, RootSigner::Key(&a), &LongIdKey, "Long")
            .await;
        assert!(
            matches!(
                added,
                Err(SdkError::SecurityKey(SecurityKeyError::Other(_)))
            ),
            "{added:?}"
        );
    }

    /// P6: an identity with every root slot taken refuses another key
    /// before the key is asked to make a credential, which would take one
    /// of the key's own resident slots for nothing.
    #[tokio::test]
    async fn a_full_identity_refuses_a_key_before_it_registers() {
        let keys: Vec<SoftSecurityKey> = (0..MAX_ROOTS).map(|_| SoftSecurityKey::new()).collect();
        let refs: Vec<&dyn SecurityKey> = keys.iter().map(|k| k as &dyn SecurityKey).collect();
        let desktop = portable();
        let id = desktop.create_with_keys(&refs, "Sam").await.unwrap();
        let extra = SoftSecurityKey::new();
        let added = desktop
            .add_root(id, RootSigner::Key(&keys[0]), &extra, "Extra")
            .await;
        assert!(matches!(added, Err(SdkError::InvalidInput(_))), "{added:?}");
        assert!(extra.credentials().is_empty(), "the key registered anyway");
    }

    #[tokio::test]
    async fn an_identity_made_with_keys_is_its_genesis_and_pairs_the_desktop_itself() {
        let (desktop, id, a, b) = key_identity().await;
        let record = desktop.identity_record(id).await.unwrap();
        let genesis = record.genesis.unwrap();
        assert_eq!(genesis.user(), id);
        assert_eq!(genesis.verify(), Ok(()));
        assert_eq!(
            genesis.roots,
            [a.credentials()[0].root(), b.credentials()[0].root()]
        );
        // Each key made one credential, for the identity's handle.
        assert_eq!(a.credentials()[0].user_handle(), Some(genesis.handle));
        let listed = desktop.list().await.unwrap();
        assert!(listed[0].key_rooted && !listed[0].has_root, "{listed:?}");

        let credential = desktop.pair_self(id, RootSigner::Key(&a)).await.unwrap();
        assert_eq!(credential.verify(unix_now()), Ok(()));
        assert_eq!(credential.signer(), a.credentials()[0].root());
        assert_eq!(credential.genesis.as_deref(), Some(&genesis));
        assert_eq!(desktop.credential(id).await.unwrap(), Some(credential));
    }

    #[tokio::test]
    async fn the_backup_key_issues_and_a_stranger_key_does_not() {
        let (desktop, id, _a, b) = key_identity().await;
        let tv = DevicePublicKey::from_bytes(
            ed25519_dalek::SigningKey::from_bytes(&[9; 32])
                .verifying_key()
                .to_bytes(),
        )
        .unwrap();
        let by_b = desktop
            .issue_credential_by(id, RootSigner::Key(&b), tv, Some("TV"))
            .await
            .unwrap();
        assert_eq!(by_b.signer(), b.credentials()[0].root());
        let stranger = SoftSecurityKey::new();
        stranger
            .register(PORTABLE_RP, [0; 32], "someone else")
            .await
            .unwrap();
        assert!(matches!(
            desktop
                .issue_credential_by(id, RootSigner::Key(&stranger), tv, None)
                .await,
            Err(SdkError::NotARoot)
        ));
        // Nor does a phrase root this device does not hold.
        assert!(matches!(
            desktop
                .issue_credential_by(id, RootSigner::Phrase, tv, None)
                .await,
            Err(SdkError::NotARoot | SdkError::UnknownIdentity(_))
        ));
    }

    #[tokio::test]
    async fn a_removed_key_issues_nothing_and_an_added_one_does() {
        let (desktop, id, a, b) = key_identity().await;
        let state = desktop
            .remove_root(id, RootSigner::Key(&b), &a.credentials()[0].root())
            .await
            .unwrap();
        assert_eq!(state.verify(), Ok(()));
        let roots = desktop.roots(id).await.unwrap();
        assert!(!roots.holds(&a.credentials()[0].root()));
        assert!(matches!(
            desktop.pair_self(id, RootSigner::Key(&a)).await,
            Err(SdkError::NotARoot)
        ));
        // B adds C, a new key; C acts at once, and the state keeps A out.
        let c = SoftSecurityKey::new();
        let added = desktop
            .add_root(id, RootSigner::Key(&b), &c, "Sam")
            .await
            .unwrap();
        assert_eq!(added.roots_removed.len(), 1);
        assert_eq!(added.roots_added.len(), 1);
        desktop.pair_self(id, RootSigner::Key(&c)).await.unwrap();
        // The last root left cannot be removed.
        desktop
            .remove_root(id, RootSigner::Key(&c), &b.credentials()[0].root())
            .await
            .unwrap();
        assert!(matches!(
            desktop
                .remove_root(id, RootSigner::Key(&c), &c.credentials()[0].root())
                .await,
            Err(SdkError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn a_phrase_identity_adds_a_key_keeps_its_id_and_the_key_issues() {
        let phone = portable();
        let (id, _phrase) = phone.create("Sam").await.unwrap();
        let key = SoftSecurityKey::new();
        let state = phone
            .add_root(id, RootSigner::Phrase, &key, "Sam")
            .await
            .unwrap();
        assert_eq!(state.user, id);
        assert_eq!(state.signer, None);
        let credential = phone.pair_self(id, RootSigner::Key(&key)).await.unwrap();
        assert_eq!(credential.user, id);
        assert_eq!(credential.signer(), key.credentials()[0].root());
        // A phrase identity carries no genesis: its id is its root.
        assert_eq!(credential.genesis, None);
    }

    fn some_device(n: u8) -> DevicePublicKey {
        DevicePublicKey::from_bytes(
            ed25519_dalek::SigningKey::from_bytes(&[n; 32])
                .verifying_key()
                .to_bytes(),
        )
        .unwrap()
    }

    /// A stranger's phone pairing as a guest already here, with a genesis
    /// of its own (or a bogus one) carrying a state its own phrase signs.
    #[tokio::test]
    async fn a_second_pairing_with_a_foreign_genesis_changes_nothing_kept() {
        let phone = portable();
        let (id, _phrase) = phone.create("Sam").await.unwrap();
        let storage = Arc::new(MemoryStorage::default());
        let tv = Portable::new(storage.clone());
        let tv_key = tv.device_key(id).await.unwrap();
        // The genuine pairing, with a state that revokes an old device.
        let genuine_state = phone.revoke(id, some_device(3)).await.unwrap();
        let genuine = phone
            .issue_credential(id, tv_key, Some("TV"))
            .await
            .unwrap();
        tv.install_with_state(genuine.clone(), Some(genuine_state.clone()))
            .await
            .unwrap();
        let revoked_before = load_revoked(storage.as_ref(), id).await.unwrap();

        let (stranger, _) = super::super::PortableRoot::generate();
        let stranger_root = RootKey::phrase_of(&stranger.id());
        let handle = [7; HANDLE_LEN];
        let message =
            IdentityGenesis::message(&handle, std::slice::from_ref(&stranger_root)).unwrap();
        // A genesis that holds, but is the stranger's identity, not `id`;
        // and one whose signature does not hold at all.
        let foreign = IdentityGenesis {
            handle,
            roots: vec![stranger_root.clone()],
            signatures: vec![stranger.sign(&message).into()],
        };
        let bogus = IdentityGenesis {
            signatures: vec![stranger.sign(b"something else").into()],
            ..foreign.clone()
        };
        let now = unix_now();
        for genesis in [foreign, bogus] {
            let forged_state = PortableIdentityState::signed_by(
                id,
                stranger_root.clone(),
                999,
                StateChanges {
                    revoked: vec![RevokedKey {
                        key: some_device(4),
                        revoked_at: now,
                    }],
                    ..StateChanges::default()
                },
                now,
                |m| stranger.sign(m).into(),
            )
            .unwrap();
            let mut forged = UserDeviceCredential::signed_by(
                id,
                stranger_root.clone(),
                tv_key,
                1,
                now,
                now + 3600,
                |m| stranger.sign(m).into(),
            );
            forged.genesis = Some(Box::new(genesis));

            let refused = tv.install_with_state(forged, Some(forged_state)).await;
            assert!(
                matches!(
                    refused,
                    Err(SdkError::Pairing(
                        crate::pairing::PairingError::CredentialRefused
                    ))
                ),
                "{refused:?}"
            );
            assert_eq!(
                tv.identity_state(id).await.unwrap(),
                Some(genuine_state.clone())
            );
            assert_eq!(
                load_revoked(storage.as_ref(), id).await.unwrap(),
                revoked_before
            );
            assert_eq!(load_genesis(storage.as_ref(), id).await.unwrap(), None);
            assert_eq!(tv.credential(id).await.unwrap(), Some(genuine.clone()));
        }
        // The guest still signs in with the credential it was given.
        let (sent, signature) = tv.prove(id, &[0x5a; 32], &[0xa5; 32]).await.unwrap();
        assert_eq!(sent, genuine);
        assert_eq!(
            spoolframe_protocol::portable::verify_device_proof(
                &sent,
                &[0x5a; 32],
                &[0xa5; 32],
                &signature
            ),
            Ok(())
        );
    }

    /// A newer state from the root, but a credential the device refuses:
    /// the state it had is put back, and no revocation joins.
    #[tokio::test]
    async fn a_refused_credential_puts_back_the_state_the_guest_had() {
        let phone = portable();
        let (id, _phrase) = phone.create("Sam").await.unwrap();
        let storage = Arc::new(MemoryStorage::default());
        let tv = Portable::new(storage.clone());
        let tv_key = tv.device_key(id).await.unwrap();
        let first = phone.revoke(id, some_device(3)).await.unwrap();
        let genuine = phone.issue_credential(id, tv_key, None).await.unwrap();
        tv.install_with_state(genuine, Some(first.clone()))
            .await
            .unwrap();
        let revoked_before = load_revoked(storage.as_ref(), id).await.unwrap();

        let newer = phone.revoke(id, some_device(4)).await.unwrap();
        let long_ago = unix_now() - MAX_CREDENTIAL_VALIDITY - 60;
        let expired = phone
            .issue_credential_at(id, tv_key, None, long_ago)
            .await
            .unwrap();
        assert!(tv.install_with_state(expired, Some(newer)).await.is_err());
        assert_eq!(tv.identity_state(id).await.unwrap(), Some(first));
        assert_eq!(
            load_revoked(storage.as_ref(), id).await.unwrap(),
            revoked_before
        );
    }

    #[tokio::test]
    async fn a_phrase_revocation_keeps_the_keys_the_identity_added() {
        let phone = portable();
        let (id, _phrase) = phone.create("Sam").await.unwrap();
        let key = SoftSecurityKey::new();
        phone
            .add_root(id, RootSigner::Phrase, &key, "Sam")
            .await
            .unwrap();
        let tv = DevicePublicKey::from_bytes(
            ed25519_dalek::SigningKey::from_bytes(&[9; 32])
                .verifying_key()
                .to_bytes(),
        )
        .unwrap();
        let state = phone.revoke(id, tv).await.unwrap();
        assert_eq!(state.revision, 2);
        assert!(state.revokes(&tv));
        assert_eq!(state.roots_added.len(), 1, "{state:?}");
        assert_eq!(state.roots_added[0].root, key.credentials()[0].root());
        // The key is still one of the identity's roots here.
        phone.pair_self(id, RootSigner::Key(&key)).await.unwrap();
    }

    #[tokio::test]
    async fn a_credential_in_its_last_fourteen_days_is_due() {
        let (desktop, id, a, _) = key_identity().await;
        let credential = desktop.pair_self(id, RootSigner::Key(&a)).await.unwrap();
        let expires = credential.expires_at;
        assert_eq!(
            desktop
                .renewal_due_at(id, expires - RENEWAL_DUE - 1)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            desktop
                .renewal_due_at(id, expires - RENEWAL_DUE)
                .await
                .unwrap(),
            Some(expires)
        );
        // Renewed with one touch: the same device key, 90 days again.
        let renewed = desktop.pair_self(id, RootSigner::Key(&a)).await.unwrap();
        assert_eq!(renewed.device_key, credential.device_key);
        assert!(renewed.serial > credential.serial);
        assert_eq!(
            renewed.expires_at - renewed.issued_at,
            MAX_CREDENTIAL_VALIDITY
        );
    }

    #[tokio::test]
    async fn a_fresh_device_with_the_record_and_the_backup_key_acts_for_the_identity() {
        let (desktop, id, a, b) = key_identity().await;
        desktop
            .remove_root(id, RootSigner::Key(&a), &a.credentials()[0].root())
            .await
            .unwrap();
        let record = desktop.identity_record(id).await.unwrap();
        // As written to a file and read back.
        let record: IdentityRecord =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();

        let fresh = portable();
        assert_eq!(
            fresh.import_record(record.clone(), "Sam").await.unwrap(),
            id
        );
        assert!(fresh.pair_self(id, RootSigner::Key(&b)).await.is_ok());
        // The removed key stays removed there too.
        assert!(matches!(
            fresh.pair_self(id, RootSigner::Key(&a)).await,
            Err(SdkError::NotARoot)
        ));

        // A record whose genesis is another identity's is refused.
        let (_, other, ..) = key_identity().await;
        let wrong = IdentityRecord {
            user: other,
            ..record.clone()
        };
        assert!(portable().import_record(wrong, "Sam").await.is_err());

        // A record whose state no root of its identity signed is bad data,
        // not "this key isn't one of theirs".
        let (_, stranger_id, stranger, _) = key_identity().await;
        let forged = PortableIdentityState::signed_by(
            id,
            stranger.credentials()[0].root(),
            9,
            StateChanges::default(),
            unix_now(),
            |m| stranger.credentials()[0].assert(m),
        )
        .unwrap();
        assert_ne!(stranger_id, id);
        let forged = IdentityRecord {
            state: Some(forged),
            ..record
        };
        assert!(matches!(
            portable().import_record(forged, "Sam").await,
            Err(SdkError::IdentityState(StateError::BadSignature))
        ));
    }

    // Apple's system security-key API (WebAuthn form).

    fn is_webauthn(signature: &RootSignature) -> bool {
        matches!(signature, RootSignature::WebAuthn { .. })
    }

    /// Every root action, with keys that answer only through WebAuthn (an
    /// iPhone's system API): the genesis, a credential, a revocation, an
    /// added and a removed root, all signed in that form, all holding.
    #[tokio::test]
    async fn an_identity_rooted_in_webauthn_keys_does_every_root_action() {
        let (a, b) = (SoftSecurityKey::webauthn(), SoftSecurityKey::webauthn());
        let phone = portable();
        let id = phone.create_with_keys(&[&a, &b], "Sam").await.unwrap();
        let genesis = phone.identity_record(id).await.unwrap().genesis.unwrap();
        assert_eq!(genesis.verify(), Ok(()));
        assert!(genesis.signatures.iter().all(is_webauthn), "{genesis:?}");
        assert_eq!(
            genesis.roots,
            [a.credentials()[0].root(), b.credentials()[0].root()]
        );
        assert_eq!(a.credentials()[0].user_handle(), Some(genesis.handle));

        let credential = phone.pair_self(id, RootSigner::Key(&a)).await.unwrap();
        assert!(is_webauthn(&credential.signature));
        assert_eq!(credential.verify(unix_now()), Ok(()));
        let tv = some_device(9);
        let issued = phone
            .issue_credential_by(id, RootSigner::Key(&b), tv, Some("TV"))
            .await
            .unwrap();
        assert_eq!(issued.signer(), b.credentials()[0].root());
        let revoked = phone.revoke_by(id, RootSigner::Key(&a), tv).await.unwrap();
        assert!(is_webauthn(&revoked.signature) && revoked.revokes(&tv));
        let c = SoftSecurityKey::webauthn();
        let added = phone
            .add_root(id, RootSigner::Key(&a), &c, "Sam")
            .await
            .unwrap();
        assert_eq!(added.verify(), Ok(()));
        let addition = &added.roots_added[0];
        assert!(is_webauthn(&addition.possession) && is_webauthn(&addition.endorsement));
        let removed = phone
            .remove_root(id, RootSigner::Key(&c), &a.credentials()[0].root())
            .await
            .unwrap();
        assert_eq!(removed.verify(), Ok(()));
        assert!(matches!(
            phone.pair_self(id, RootSigner::Key(&a)).await,
            Err(SdkError::NotARoot)
        ));
    }

    /// One key, two devices: registered through libfido2 on Linux, it
    /// acts through the system API on an iPhone, and the reverse.
    #[tokio::test]
    async fn a_key_registered_on_linux_acts_through_webauthn_and_the_reverse() {
        let on_linux = SoftSecurityKey::new();
        let desktop = portable();
        let id = desktop.create_with_keys(&[&on_linux], "Sam").await.unwrap();
        let record = desktop.identity_record(id).await.unwrap();
        let iphone = portable();
        iphone.import_record(record, "Sam").await.unwrap();
        let credential = iphone
            .pair_self(id, RootSigner::Key(&on_linux.as_webauthn()))
            .await
            .unwrap();
        assert!(is_webauthn(&credential.signature));
        assert_eq!(credential.signer(), on_linux.credentials()[0].root());
        assert_eq!(credential.verify(unix_now()), Ok(()));

        let on_iphone = SoftSecurityKey::webauthn();
        let phone = portable();
        let id = phone.create_with_keys(&[&on_iphone], "Sam").await.unwrap();
        let record = phone.identity_record(id).await.unwrap();
        let linux = portable();
        linux.import_record(record, "Sam").await.unwrap();
        let credential = linux
            .pair_self(id, RootSigner::Key(&on_iphone.as_ctap()))
            .await
            .unwrap();
        assert!(matches!(credential.signature, RootSignature::Ctap { .. }));
        assert_eq!(credential.signer(), on_iphone.credentials()[0].root());
    }

    /// A key whose registration is made for another identity, or whose
    /// assertion is of the wrong type; or one that fails as Apple's API
    /// fails, with no detail.
    struct Misbehaving {
        key: SoftKey,
        how: Misbehaviour,
    }

    #[derive(Clone, Copy)]
    enum Misbehaviour {
        RegistersForAnotherHandle,
        SignsAsRegistration,
        Cancels,
        Fails,
    }

    impl Misbehaving {
        fn new(how: Misbehaviour) -> Self {
            Self {
                key: SoftKey::from_seed(7),
                how,
            }
        }
    }

    #[async_trait]
    impl SecurityKey for Misbehaving {
        async fn register_root(
            &self,
            _: &str,
            handle: [u8; HANDLE_LEN],
            _: &str,
        ) -> Result<RootRegistration, SdkError> {
            match self.how {
                Misbehaviour::RegistersForAnotherHandle => {
                    let mut other = handle;
                    other[0] ^= 1;
                    Ok(self.key.register_webauthn(&other))
                }
                Misbehaviour::SignsAsRegistration => Ok(self.key.register_webauthn(&handle)),
                Misbehaviour::Cancels => Err(SecurityKeyError::Cancelled.into()),
                Misbehaviour::Fails => Err(SecurityKeyError::Failed(None).into()),
            }
        }

        async fn assert(
            &self,
            _: &str,
            challenge: [u8; 32],
            _: &[Vec<u8>],
        ) -> Result<Vec<KeyAssertion>, SdkError> {
            use spoolframe_protocol::portable::soft_key::{UP_UV, client_data_json};
            Ok(vec![KeyAssertion {
                credential_id: self.key.credential_id().to_vec(),
                signature: self
                    .key
                    .assert_webauthn_with(client_data_json("webauthn.create", &challenge), UP_UV),
                user_handle: None,
            }])
        }
    }

    #[tokio::test]
    async fn a_webauthn_registration_for_another_identity_is_refused_and_nothing_kept() {
        let phone = portable();
        let made = phone
            .create_with_keys(
                &[&Misbehaving::new(Misbehaviour::RegistersForAnotherHandle)],
                "Sam",
            )
            .await;
        assert!(
            matches!(
                made,
                Err(SdkError::SecurityKey(SecurityKeyError::BadAssertion))
            ),
            "{made:?}"
        );
        assert!(phone.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_registration_presented_as_an_assertion_is_refused() {
        let made = portable()
            .create_with_keys(
                &[&Misbehaving::new(Misbehaviour::SignsAsRegistration)],
                "Sam",
            )
            .await;
        assert!(
            matches!(
                made,
                Err(SdkError::SecurityKey(SecurityKeyError::BadAssertion))
            ),
            "{made:?}"
        );
    }

    /// Apple's API says only "cancelled" or "failed": the SDK passes that
    /// on as it is.
    #[tokio::test]
    async fn a_plain_cancel_or_failure_comes_back_as_it_was_said() {
        let (desktop, id, a, _) = key_identity().await;
        for (how, said) in [
            (Misbehaviour::Cancels, SecurityKeyError::Cancelled),
            (Misbehaviour::Fails, SecurityKeyError::Failed(None)),
        ] {
            let added = desktop
                .add_root(id, RootSigner::Key(&a), &Misbehaving::new(how), "K")
                .await;
            assert!(
                matches!(&added, Err(SdkError::SecurityKey(e)) if *e == said),
                "{added:?}"
            );
        }
        assert_eq!(
            SecurityKeyError::Failed(Some("the NFC session ended".into())).to_string(),
            "the security key failed: the NFC session ended"
        );
        assert_eq!(
            SecurityKeyError::Failed(None).to_string(),
            "the security key failed"
        );
    }

    /// A key that implements neither way to register fails plainly
    /// rather than looping.
    struct AssertOnly;

    #[async_trait]
    impl SecurityKey for AssertOnly {
        async fn assert(
            &self,
            _: &str,
            _: [u8; 32],
            _: &[Vec<u8>],
        ) -> Result<Vec<KeyAssertion>, SdkError> {
            Err(SecurityKeyError::NoCredential.into())
        }
    }

    #[tokio::test]
    async fn a_key_that_cannot_register_fails_plainly() {
        let made = portable().create_with_keys(&[&AssertOnly], "Sam").await;
        assert!(
            matches!(
                made,
                Err(SdkError::SecurityKey(SecurityKeyError::Failed(Some(_))))
            ),
            "{made:?}"
        );
    }
}
