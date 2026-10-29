//! A portable user's identity: the public
//! half of a root signing key the user owns, not an account any server
//! hosts; and what that root signs: a credential for
//! each of the user's devices, and the state that revokes them; and the
//! proof that ties a local account to the identity.
//!
//! Only public things live here. The root's secret, and the recovery
//! phrase it comes from, are the client SDK's (`spoolframe_client_sdk::
//! portable`) and never travel: a server checks what the root signed with
//! [`PortableUserId::verify`] and holds nothing it could sign with.

use std::fmt;
use std::str::FromStr;

use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use ed25519_dalek::Signature;

use crate::fingerprint;

mod client_data;
mod roots;
pub(crate) use roots::handle_hex;
#[cfg(any(test, feature = "soft-key"))]
pub mod soft_key;

pub use roots::{
    AddedRoot, GENESIS_DOMAIN, GenesisError, HANDLE_LEN, IdentityGenesis, MAX_ATTESTATION_OBJECT,
    MAX_AUTHENTICATOR_DATA, MAX_CLIENT_DATA_JSON, MAX_CREDENTIAL_ID, MAX_DER_SIGNATURE,
    MAX_ROOT_CHANGES, MAX_ROOTS, P256_PUBLIC_KEY_LEN, PORTABLE_RP, ROOT_ADD_DOMAIN,
    ROOT_ENDORSE_DOMAIN, RemovedRoot, RootChanges, RootKey, RootRegistration, RootSet,
    RootSignature, USER_ID_CONTEXT, WebAuthnError, portable_rp_id_hash, root_add_message,
    root_endorse_message,
};

/// Bytes in a [`PortableUserId`]: an Ed25519 public key.
pub const PORTABLE_USER_ID_LEN: usize = 32;

/// Who a portable user is. For an identity begun with one phrase root, its
/// root's Ed25519 public key; for any other, derived from its
/// genesis ([`IdentityGenesis::user`]). Written as 64 lowercase hex
/// digits, as iroh writes an `EndpointId`; serialized the same way.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortableUserId([u8; PORTABLE_USER_ID_LEN]);

/// Not a portable user id: not 64 hex digits.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a portable user id: {0}")]
pub struct InvalidPortableUserId(&'static str);

impl PortableUserId {
    /// The id `bytes` are. Any 32 bytes: an id derived from a genesis
    /// is a hash, not a curve point. A phrase identity's is its
    /// root's public key, and [`Self::verify`] fails for one that is not
    /// a point.
    pub fn from_bytes(bytes: [u8; PORTABLE_USER_ID_LEN]) -> Result<Self, InvalidPortableUserId> {
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; PORTABLE_USER_ID_LEN] {
        &self.0
    }

    /// The short code a person compares between two screens, in the same
    /// format as a server's ([`fingerprint`]): `K7QX-3MPA-9TRD-WF2H`.
    pub fn fingerprint(&self) -> String {
        fingerprint::fingerprint(&self.0)
    }

    /// Whether `signature` is this user's phrase root's over `message`,
    /// the root a phrase identity's id is. Strict Ed25519 verification: a
    /// signature another could forge from a valid one, or one under a
    /// weak key, does not pass. An identity with other roots checks them
    /// with [`RootKey::verify`].
    pub fn verify(&self, message: &[u8], signature: &Signature) -> bool {
        VerifyingKey::from_bytes(&self.0)
            .is_ok_and(|key| key.verify_strict(message, signature).is_ok())
    }
}

impl fmt::Display for PortableUserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0)
    }
}

impl fmt::Debug for PortableUserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PortableUserId({self})")
    }
}

impl FromStr for PortableUserId {
    type Err = InvalidPortableUserId;

    /// 64 hex digits, either case.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_bytes(parse_hex(s).ok_or(InvalidPortableUserId("not 64 hex digits"))?)
    }
}

impl Serialize for PortableUserId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PortableUserId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// `bytes` as lowercase hex.
fn write_hex(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    for byte in bytes {
        write!(f, "{byte:02x}")?;
    }
    Ok(())
}

/// Exactly `2 × N` hex digits, either case, as bytes.
fn parse_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != 2 * N || !s.is_ascii() {
        return None;
    }
    let mut bytes = [0u8; N];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(bytes)
}

/// Bytes in a [`DevicePublicKey`]: an Ed25519 public key.
pub const DEVICE_PUBLIC_KEY_LEN: usize = 32;

/// One device's key for acting as a portable user: its own Ed25519
/// keypair, apart from both the user's root and the device's Iroh
/// endpoint key. Written, and serialized, as 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DevicePublicKey([u8; DEVICE_PUBLIC_KEY_LEN]);

/// Not a device key: not 64 hex digits, or not an Ed25519 public key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a device public key: {0}")]
pub struct InvalidDevicePublicKey(&'static str);

impl DevicePublicKey {
    /// The key `bytes` are; an error if they are not a point on the curve.
    pub fn from_bytes(bytes: [u8; DEVICE_PUBLIC_KEY_LEN]) -> Result<Self, InvalidDevicePublicKey> {
        VerifyingKey::from_bytes(&bytes)
            .map_err(|_| InvalidDevicePublicKey("not an Ed25519 public key"))?;
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; DEVICE_PUBLIC_KEY_LEN] {
        &self.0
    }

    /// The short code a person compares between the device and the phone
    /// that authorizes it, in the same format as an identity's
    /// ([`PortableUserId::fingerprint`]).
    pub fn fingerprint(&self) -> String {
        fingerprint::fingerprint(&self.0)
    }

    /// Whether `signature` is this device key's over `message`, strictly
    /// (as [`PortableUserId::verify`]).
    pub fn verify(&self, message: &[u8], signature: &Signature) -> bool {
        VerifyingKey::from_bytes(&self.0)
            .is_ok_and(|key| key.verify_strict(message, signature).is_ok())
    }
}

impl fmt::Display for DevicePublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0)
    }
}

impl fmt::Debug for DevicePublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DevicePublicKey({self})")
    }
}

impl FromStr for DevicePublicKey {
    type Err = InvalidDevicePublicKey;

    /// 64 hex digits, either case.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_bytes(parse_hex(s).ok_or(InvalidDevicePublicKey("not 64 hex digits"))?)
    }
}

impl Serialize for DevicePublicKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DevicePublicKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// A signature in serde: 128 lowercase hex digits (64 bytes).
pub(crate) mod signature_hex {
    use super::*;

    struct Hex<'a>(&'a Signature);

    impl fmt::Display for Hex<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write_hex(f, &self.0.to_bytes())
        }
    }

    pub fn serialize<S: Serializer>(signature: &Signature, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&Hex(signature))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Signature, D::Error> {
        let text = String::deserialize(d)?;
        parse_hex::<{ Signature::BYTE_SIZE }>(&text)
            .map(|bytes| Signature::from_bytes(&bytes))
            .ok_or_else(|| serde::de::Error::custom("a signature is 128 hex digits"))
    }
}

/// Seconds in a day.
const DAY: i64 = 24 * 60 * 60;

/// The longest a credential may be good for: 90 days from its
/// `issued_at`. A stale device ages out
/// within that, even where no revocation ever reaches.
pub const MAX_CREDENTIAL_VALIDITY: i64 = 90 * DAY;

/// How far ahead of a verifier's clock a credential's `issued_at` may be:
/// the phone's clock and the server's need not agree to the second.
pub const CLOCK_SKEW: i64 = 5 * 60;

/// The domain the root's signature over a credential is made in, ahead of
/// the bytes [`UserDeviceCredential::message`] lays out. Fixed forever: a
/// new layout takes a new string.
pub const CREDENTIAL_DOMAIN: &[u8] = b"spoolframe device credential v1\0";

/// The domain of the root's signature over an identity state. v2 added
/// each key's `revoked_at`; v1 was never sent anywhere.
pub const STATE_DOMAIN: &[u8] = b"spoolframe identity state v2\0";

/// The domain of a device key's proof of possession to one server.
pub const DEVICE_PROOF_DOMAIN: &[u8] = b"spoolframe device proof v1\0";

/// The most device keys an identity state may revoke. Revocations older
/// than [`REVOCATION_WINDOW`] may be dropped, so this bounds the live ones
/// only.
pub const MAX_REVOKED: usize = 1024;

/// How long a revocation matters: a key revoked longer ago than this has
/// no credential left that holds. Every credential expires within
/// [`MAX_CREDENTIAL_VALIDITY`] of its `issued_at`, which is at most
/// [`CLOCK_SKEW`] ahead of the clock it was revoked by, and the root
/// issues none to a key it has revoked.
pub const REVOCATION_WINDOW: i64 = MAX_CREDENTIAL_VALIDITY + CLOCK_SKEW;

/// Whether a revocation made at `revoked_at` is past [`REVOCATION_WINDOW`]
/// at `now`, and so may be dropped: it can stop no credential that would
/// otherwise hold.
pub fn revocation_lapsed(revoked_at: i64, now: i64) -> bool {
    revoked_at < now.saturating_sub(REVOCATION_WINDOW)
}

/// Why a credential, or a device's proof with it, does not authenticate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum CredentialError {
    /// The root it names did not sign it, as it is: tampered, or signed by
    /// another identity's root.
    #[error("the credential is not signed by the identity it names")]
    BadSignature,
    /// Issued further ahead of this clock than [`CLOCK_SKEW`].
    #[error("the credential is not valid yet")]
    NotYetValid,
    /// Past its `expires_at`.
    #[error("the credential has expired")]
    Expired,
    /// Good for longer than [`MAX_CREDENTIAL_VALIDITY`]: no root this
    /// code signs with makes one.
    #[error("the credential is good for longer than a credential may be")]
    TooLong,
    /// The device key did not sign the proof: whoever presents the
    /// credential does not hold its device's key.
    #[error("the device key did not sign the proof")]
    BadDeviceProof,
}

/// A user's root's word that a device key may act as them, until
/// `expires_at`. The server checks it alone, with no account server: the
/// root signed it, it is in date, the device holds its key
/// ([`verify_device_proof`]) and no state the server has seen revokes the
/// key ([`PortableIdentityState::revokes`]).
///
/// Times are Unix seconds. Renewal is a new credential for the same key,
/// with a larger serial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserDeviceCredential {
    pub user: PortableUserId,
    pub device_key: DevicePublicKey,
    /// Larger for a later credential from one holder of the root, so two
    /// for one key differ. Only the signature reads it; the SDK gives the
    /// issue time in milliseconds, or one past its last if that is larger.
    pub serial: u64,
    pub issued_at: i64,
    /// Required: every credential expires.
    pub expires_at: i64,
    /// The root's, over [`Self::message`]: Ed25519 from a phrase root; a
    /// CTAP2 assertion from a security key.
    pub signature: RootSignature,
    /// The root that signed it; absent, the phrase root the
    /// user's id is. Whether it is one
    /// of the user's roots is the verifier's to check, against the
    /// identity's [`RootSet`].
    // Left out when none, not for older peers: a server and a device keep
    // these as JSON, and older records have no key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<RootKey>,
    /// The identity's genesis, carried for a verifier that has not seen
    /// it: an identity with a security key root cannot be checked
    /// without it. Not signed: it proves itself, its hash being the id.
    // Left out when none, not for older peers: a server and a device keep
    // these as JSON, and older records have no key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genesis: Option<Box<IdentityGenesis>>,
}

impl UserDeviceCredential {
    /// A credential `sign` signs with `user`'s phrase root: the client
    /// SDK's root, which this crate never holds.
    pub fn new(
        user: PortableUserId,
        device_key: DevicePublicKey,
        serial: u64,
        issued_at: i64,
        expires_at: i64,
        sign: impl FnOnce(&[u8]) -> Signature,
    ) -> Self {
        let signature = sign(&Self::message(
            &user,
            &device_key,
            serial,
            issued_at,
            expires_at,
        ));
        Self {
            user,
            device_key,
            serial,
            issued_at,
            expires_at,
            signature: RootSignature::Ed25519(signature),
            root: None,
            genesis: None,
        }
    }

    /// A credential `root` signs with `sign`, any kind of root.
    /// A phrase identity's own phrase root is left unnamed, as before.
    #[allow(clippy::too_many_arguments)]
    pub fn signed_by(
        user: PortableUserId,
        root: RootKey,
        device_key: DevicePublicKey,
        serial: u64,
        issued_at: i64,
        expires_at: i64,
        sign: impl FnOnce(&[u8]) -> RootSignature,
    ) -> Self {
        let signature = sign(&Self::message(
            &user,
            &device_key,
            serial,
            issued_at,
            expires_at,
        ));
        let root = (root != RootKey::phrase_of(&user)).then_some(root);
        Self {
            user,
            device_key,
            serial,
            issued_at,
            expires_at,
            signature,
            root,
            genesis: None,
        }
    }

    /// The root it says signed it: the one it names, or the phrase root
    /// its user's id is.
    pub fn signer(&self) -> RootKey {
        self.root
            .clone()
            .unwrap_or_else(|| RootKey::phrase_of(&self.user))
    }

    /// What the root signs: [`CREDENTIAL_DOMAIN`], then user (32 bytes),
    /// device key (32), serial (u64), issued_at (i64), expires_at (i64),
    /// the integers big-endian. Fixed width throughout, so there is one
    /// encoding of each credential and nothing to canonicalise.
    pub fn message(
        user: &PortableUserId,
        device_key: &DevicePublicKey,
        serial: u64,
        issued_at: i64,
        expires_at: i64,
    ) -> Vec<u8> {
        let mut message = Vec::with_capacity(CREDENTIAL_DOMAIN.len() + 32 + 32 + 8 * 3);
        message.extend_from_slice(CREDENTIAL_DOMAIN);
        message.extend_from_slice(user.as_bytes());
        message.extend_from_slice(device_key.as_bytes());
        message.extend_from_slice(&serial.to_be_bytes());
        message.extend_from_slice(&issued_at.to_be_bytes());
        message.extend_from_slice(&expires_at.to_be_bytes());
        message
    }

    /// Whether it is good at `now` (Unix seconds): signed by the root it
    /// names ([`Self::signer`]), issued no later than `now` +
    /// [`CLOCK_SKEW`], not yet expired, and good for no longer than
    /// [`MAX_CREDENTIAL_VALIDITY`]. Says nothing of whether that root is
    /// one of the user's ([`RootSet`]) nor of revocation, which are the
    /// verifier's, from the states it holds.
    pub fn verify(&self, now: i64) -> Result<(), CredentialError> {
        let message = Self::message(
            &self.user,
            &self.device_key,
            self.serial,
            self.issued_at,
            self.expires_at,
        );
        if !self.signer().verify(&message, &self.signature) {
            return Err(CredentialError::BadSignature);
        }
        if self
            .expires_at
            .checked_sub(self.issued_at)
            .is_none_or(|validity| validity > MAX_CREDENTIAL_VALIDITY)
        {
            return Err(CredentialError::TooLong);
        }
        if self.issued_at > now.saturating_add(CLOCK_SKEW) {
            return Err(CredentialError::NotYetValid);
        }
        if now >= self.expires_at {
            return Err(CredentialError::Expired);
        }
        Ok(())
    }
}

/// What a device signs to prove to one server that it holds its key:
/// [`DEVICE_PROOF_DOMAIN`], the server's endpoint id (32 bytes), then the
/// server's nonce (32). Naming the server keeps a proof made for one from
/// being replayed to another; the nonce, one the server just chose, keeps
/// it from being replayed to the same one.
pub fn device_proof_message(server: &[u8; 32], nonce: &[u8; 32]) -> Vec<u8> {
    let mut message = Vec::with_capacity(DEVICE_PROOF_DOMAIN.len() + 64);
    message.extend_from_slice(DEVICE_PROOF_DOMAIN);
    message.extend_from_slice(server);
    message.extend_from_slice(nonce);
    message
}

/// Whether `signature` is `credential`'s device key's over
/// [`device_proof_message`]`(server, nonce)`. Checks the proof only: the
/// credential itself is [`UserDeviceCredential::verify`]'s.
pub fn verify_device_proof(
    credential: &UserDeviceCredential,
    server: &[u8; 32],
    nonce: &[u8; 32],
    signature: &Signature,
) -> Result<(), CredentialError> {
    if credential
        .device_key
        .verify(&device_proof_message(server, nonce), signature)
    {
        Ok(())
    } else {
        Err(CredentialError::BadDeviceProof)
    }
}

/// Why an identity state is not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum StateError {
    /// The root it names (or the phrase root its user's id is) did not
    /// sign it, as it is.
    #[error("the identity state is not signed by the identity it names")]
    BadSignature,
    /// It lists more than [`MAX_REVOKED`] keys.
    #[error("the identity state revokes more than {MAX_REVOKED} device keys")]
    TooManyRevoked,
    /// It adds or removes more than [`MAX_ROOT_CHANGES`] roots.
    #[error("the identity state changes more than {MAX_ROOT_CHANGES} roots")]
    TooManyRootChanges,
    /// A root it adds is not a valid key, did not sign its addition, or
    /// its adder did not endorse it.
    #[error("a root the identity state adds does not prove it is held, or who added it")]
    BadPossession,
}

/// One revoked device key, and when the root revoked it (Unix seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RevokedKey {
    pub key: DevicePublicKey,
    pub revoked_at: i64,
}

/// `entries` as a state signs them: sorted by key, one per key, the
/// earliest `revoked_at` where a key repeats.
fn canonical(entries: &[RevokedKey]) -> Vec<RevokedKey> {
    let mut entries = entries.to_vec();
    entries.sort_unstable_by_key(|entry| (entry.key, entry.revoked_at));
    entries.dedup_by_key(|entry| entry.key);
    entries
}

/// `added` as a state signs it: sorted by root, one per root.
fn canonical_added(added: &[AddedRoot]) -> Vec<AddedRoot> {
    let mut added = added.to_vec();
    added.sort_by(|a, b| a.root.cmp(&b.root));
    added.dedup_by(|a, b| a.root == b.root);
    added
}

/// `removed` as a state signs it: sorted by root, one per root, the
/// earliest `removed_at` where a root repeats.
fn canonical_removed(removed: &[RemovedRoot]) -> Vec<RemovedRoot> {
    let mut removed = removed.to_vec();
    removed.sort_by(|a, b| (&a.root, a.removed_at).cmp(&(&b.root, b.removed_at)));
    removed.dedup_by(|a, b| a.root == b.root);
    removed
}

/// The domain of a state that names its signer or changes roots.
/// One that does neither signs [`STATE_DOMAIN`]'s layout, as before.
pub const STATE_V3_DOMAIN: &[u8] = b"spoolframe identity state v3\0";

/// A user's root's list of revoked device keys, at a revision, and
/// the roots it has added and removed. Revocation only grows: a
/// verifier checks credentials against the union of every valid state it
/// has seen for the user, and keeps the newest (by revision) to hand on.
/// A credential is good unless that union revokes its device key: adding
/// a device needs no new state, so it
/// lists no authorized keys.
///
/// Root changes are cumulative too: a state lists every root added and
/// removed so far, and a verifier works out the roots from all of them
/// ([`RootSet`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableIdentityState {
    pub user: PortableUserId,
    /// Higher is newer; each state the root makes is the last plus one.
    pub revision: u64,
    /// Sorted by key, one per key, as [`Self::new`] makes it; the
    /// signature covers the set, whatever its order here.
    pub revoked: Vec<RevokedKey>,
    /// Unix seconds. Not used to order states: the revision is.
    pub issued_at: i64,
    /// The signer's, over [`Self::message`].
    pub signature: RootSignature,
    /// The root that signed it; absent, the phrase root the
    /// user's id is.
    // Left out when none, not for older peers: a server and a device keep
    // these as JSON, and older records have no key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer: Option<RootKey>,
    /// Every root added so far, each with its proof that it is held.
    // Left out when none, not for older peers: a server and a device keep
    // these as JSON, and older records have no key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots_added: Vec<AddedRoot>,
    /// Every root removed so far.
    // Left out when none, not for older peers: a server and a device keep
    // these as JSON, and older records have no key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots_removed: Vec<RemovedRoot>,
}

/// What a state changes, for [`PortableIdentityState::signed_by`]:
/// revocations and roots, each cumulative.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateChanges {
    pub revoked: Vec<RevokedKey>,
    pub roots_added: Vec<AddedRoot>,
    pub roots_removed: Vec<RemovedRoot>,
}

impl PortableIdentityState {
    /// A state `sign` signs with `user`'s phrase root. `revoked` is put
    /// in [`Self::message`]'s order, one entry per key (the earliest);
    /// more than [`MAX_REVOKED`] is refused. Dropping lapsed entries
    /// ([`revocation_lapsed`]) is the caller's.
    pub fn new(
        user: PortableUserId,
        revision: u64,
        revoked: Vec<RevokedKey>,
        issued_at: i64,
        sign: impl FnOnce(&[u8]) -> Signature,
    ) -> Result<Self, StateError> {
        Self::signed_by(
            user,
            RootKey::phrase_of(&user),
            revision,
            StateChanges {
                revoked,
                ..StateChanges::default()
            },
            issued_at,
            |message| RootSignature::Ed25519(sign(message)),
        )
    }

    /// A state `signer` signs with `sign`: any kind of root, and
    /// any root changes. Lists are put in [`Self::message`]'s order, one
    /// entry each; over the limits is refused. A phrase identity's own
    /// phrase root, changing no roots, signs the [`STATE_DOMAIN`] layout.
    pub fn signed_by(
        user: PortableUserId,
        signer: RootKey,
        revision: u64,
        changes: StateChanges,
        issued_at: i64,
        sign: impl FnOnce(&[u8]) -> RootSignature,
    ) -> Result<Self, StateError> {
        let revoked = canonical(&changes.revoked);
        let roots_added = canonical_added(&changes.roots_added);
        let roots_removed = canonical_removed(&changes.roots_removed);
        if revoked.len() > MAX_REVOKED {
            return Err(StateError::TooManyRevoked);
        }
        if roots_added.len() > MAX_ROOT_CHANGES || roots_removed.len() > MAX_ROOT_CHANGES {
            return Err(StateError::TooManyRootChanges);
        }
        let signer = (signer != RootKey::phrase_of(&user)).then_some(signer);
        let mut state = Self {
            user,
            revision,
            revoked,
            issued_at,
            // Replaced below, once the message is laid out.
            signature: RootSignature::Ed25519(Signature::from_bytes(&[0; 64])),
            signer,
            roots_added,
            roots_removed,
        };
        state.signature = sign(&state.signed_message());
        Ok(state)
    }

    /// The root it says signed it: the one it names, or the phrase root
    /// its user's id is.
    pub fn signer(&self) -> RootKey {
        self.signer
            .clone()
            .unwrap_or_else(|| RootKey::phrase_of(&self.user))
    }

    /// Its root changes, as [`RootSet`] reads them.
    pub fn root_changes(&self) -> (RootKey, &[AddedRoot], &[RemovedRoot]) {
        (self.signer(), &self.roots_added, &self.roots_removed)
    }

    /// What the root signs: [`STATE_DOMAIN`], then user
    /// (32 bytes), revision (u64), issued_at (i64), the count of entries
    /// (u32), then the entries sorted ascending by key, one per key, each
    /// the key (32 bytes) and its revoked_at (i64); the integers
    /// big-endian. `revoked` is taken as a set; a key given twice counts
    /// once, with its earliest `revoked_at`.
    pub fn message(
        user: &PortableUserId,
        revision: u64,
        revoked: &[RevokedKey],
        issued_at: i64,
    ) -> Vec<u8> {
        let mut message = Vec::new();
        Self::lay_out(
            &mut message,
            STATE_DOMAIN,
            user,
            revision,
            revoked,
            issued_at,
        );
        message
    }

    fn lay_out(
        message: &mut Vec<u8>,
        domain: &[u8],
        user: &PortableUserId,
        revision: u64,
        revoked: &[RevokedKey],
        issued_at: i64,
    ) {
        let entries = canonical(revoked);
        let count = u32::try_from(entries.len()).expect("fewer than 2^32 keys in memory");
        message.reserve(domain.len() + 32 + 8 + 8 + 4 + 40 * entries.len());
        message.extend_from_slice(domain);
        message.extend_from_slice(user.as_bytes());
        message.extend_from_slice(&revision.to_be_bytes());
        message.extend_from_slice(&issued_at.to_be_bytes());
        message.extend_from_slice(&count.to_be_bytes());
        for entry in entries {
            message.extend_from_slice(entry.key.as_bytes());
            message.extend_from_slice(&entry.revoked_at.to_be_bytes());
        }
    }

    /// What its signer signed. [`Self::message`] for a state of
    /// the phrase root its user's id is that changes no roots; else
    /// [`STATE_V3_DOMAIN`], the same fields, then the signer
    /// ([`RootKey::bytes`]), the count of roots added (u8) and each sorted
    /// ascending by root, as the root, its adder and its added_at (i64),
    /// and the count of roots removed (u8) and each root sorted ascending
    /// with its removed_at (i64). The additions' signatures are not in
    /// it: each is checked against its own message.
    pub fn signed_message(&self) -> Vec<u8> {
        if self.signer.is_none() && self.roots_added.is_empty() && self.roots_removed.is_empty() {
            return Self::message(&self.user, self.revision, &self.revoked, self.issued_at);
        }
        let mut message = Vec::new();
        Self::lay_out(
            &mut message,
            STATE_V3_DOMAIN,
            &self.user,
            self.revision,
            &self.revoked,
            self.issued_at,
        );
        message.extend_from_slice(&self.signer().bytes());
        let added = canonical_added(&self.roots_added);
        message.push(u8::try_from(added.len()).unwrap_or(u8::MAX));
        for root in &added {
            message.extend_from_slice(&root.root.bytes());
            message.extend_from_slice(&root.added_by.bytes());
            message.extend_from_slice(&root.added_at.to_be_bytes());
        }
        let removed = canonical_removed(&self.roots_removed);
        message.push(u8::try_from(removed.len()).unwrap_or(u8::MAX));
        for root in &removed {
            message.extend_from_slice(&root.root.bytes());
            message.extend_from_slice(&root.removed_at.to_be_bytes());
        }
        message
    }

    /// Whether the root it names signed it, it is within the limits, and
    /// every root it adds is valid and signed its addition. The counts
    /// are checked first, on the lists as given, so an oversized one
    /// costs no sorting. Whether its signer is one of the user's roots is
    /// the verifier's ([`RootSet`]).
    pub fn verify(&self) -> Result<(), StateError> {
        if self.revoked.len() > MAX_REVOKED {
            return Err(StateError::TooManyRevoked);
        }
        if self.roots_added.len() > MAX_ROOT_CHANGES || self.roots_removed.len() > MAX_ROOT_CHANGES
        {
            return Err(StateError::TooManyRootChanges);
        }
        if !self
            .roots_added
            .iter()
            .all(|added| added.verify(&self.user))
        {
            return Err(StateError::BadPossession);
        }
        if self
            .signer()
            .verify(&self.signed_message(), &self.signature)
        {
            Ok(())
        } else {
            Err(StateError::BadSignature)
        }
    }

    /// Whether it revokes `key`.
    pub fn revokes(&self, key: &DevicePublicKey) -> bool {
        self.revoked.iter().any(|entry| entry.key == *key)
    }

    /// Whether `other` says the same as this: the same signed bytes and
    /// signature, whatever order its lists are in.
    pub fn same_as(&self, other: &PortableIdentityState) -> bool {
        self.signature == other.signature
            && self.signer() == other.signer()
            && self.signed_message() == other.signed_message()
    }

    /// BLAKE3 of what its signer signed and of its signature as CBOR (the
    /// wire's codec) writes it: one value for one state, whatever order
    /// its lists are in. Two states of one revision are ordered by it, the
    /// lower first ([`newest_first`]), so every verifier picks the same
    /// one as the newest.
    pub fn digest(&self) -> [u8; 32] {
        let mut signature = Vec::new();
        ciborium::into_writer(&self.signature, &mut signature)
            .expect("a signature always serializes");
        let mut digest = blake3::Hasher::new();
        digest.update(&self.signed_message());
        digest.update(&signature);
        digest.finalize().into()
    }

    /// Whether `others` carry every root change this state makes: each
    /// root it adds is added in one of them, and each root it removes is
    /// removed in one of them, no later. A state whose changes others
    /// carry adds nothing to the root set they make. Its revocations are
    /// not looked at.
    pub fn root_changes_covered_by(&self, others: &[PortableIdentityState]) -> bool {
        self.roots_added.iter().all(|added| {
            others
                .iter()
                .any(|o| o.roots_added.iter().any(|a| a.root == added.root))
        }) && self.roots_removed.iter().all(|removed| {
            others.iter().any(|o| {
                o.roots_removed
                    .iter()
                    .any(|r| r.root == removed.root && r.removed_at <= removed.removed_at)
            })
        })
    }
}

/// Newest first: the higher revision, then the lower
/// [`PortableIdentityState::digest`]. How a verifier picks the newest of
/// rival states, the same everywhere.
pub fn newest_first(a: &PortableIdentityState, b: &PortableIdentityState) -> std::cmp::Ordering {
    b.revision
        .cmp(&a.revision)
        .then_with(|| a.digest().cmp(&b.digest()))
}

/// The roots of `user`, from its genesis (none for a phrase identity
/// whose genesis the verifier has not seen: its one root is the phrase
/// root its id is) and `states`, each already checked
/// ([`PortableIdentityState::verify`]) and of `user`.
pub fn root_set(
    user: &PortableUserId,
    genesis: Option<&IdentityGenesis>,
    states: &[PortableIdentityState],
) -> RootSet {
    let first = match genesis {
        Some(genesis) => genesis.roots.clone(),
        None => vec![RootKey::phrase_of(user)],
    };
    let signers: Vec<RootKey> = states.iter().map(PortableIdentityState::signer).collect();
    let changes: Vec<RootChanges<'_>> = states
        .iter()
        .zip(&signers)
        .map(|(state, signer)| RootChanges {
            signer,
            added: &state.roots_added,
            removed: &state.roots_removed,
        })
        .collect();
    RootSet::of(&first, &changes)
}

/// What a device or server keeps of an identity to check it, and hands
/// on so another can: its genesis, absent for a phrase identity
/// (its one root is its id), and its newest state, if any. Recovery with
/// a root needs it from anywhere that has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityRecord {
    pub user: PortableUserId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genesis: Option<IdentityGenesis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<PortableIdentityState>,
}

/// The domain of the signature that ties a local account to a portable
/// identity.
pub const TIE_DOMAIN: &[u8] = b"spoolframe tie v1\0";

/// What a portable user signs to tie their identity to the local account
/// `username` on the server whose endpoint id is `server`, answering the
/// server's `nonce`: [`TIE_DOMAIN`], the server's id (32 bytes),
/// the nonce (32), the user (32), then the username's length in bytes
/// (u16, big-endian) and its UTF-8. Each part keeps a proof from being
/// used elsewhere: on another server, again on this one, for another
/// identity, or onto another account here.
///
/// `None` for a username of more than 65,535 bytes, which no server has
/// (a username is at most 64 characters).
pub fn tie_message(
    server: &[u8; 32],
    nonce: &[u8; 32],
    user: &PortableUserId,
    username: &str,
) -> Option<Vec<u8>> {
    let length = u16::try_from(username.len()).ok()?;
    let mut message = Vec::with_capacity(TIE_DOMAIN.len() + 96 + 2 + username.len());
    message.extend_from_slice(TIE_DOMAIN);
    message.extend_from_slice(server);
    message.extend_from_slice(nonce);
    message.extend_from_slice(user.as_bytes());
    message.extend_from_slice(&length.to_be_bytes());
    message.extend_from_slice(username.as_bytes());
    Some(message)
}

/// Who signed a [`tie_message`] for a portable user: their root, on the
/// phone that holds it, or one of their device keys, with the credential
/// the root gave it. Either way the root itself never travels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TieProof {
    /// A root's signature: the phrase root the user's id is, or
    /// the root it names, which the verifier checks is one of the user's.
    Root {
        signature: RootSignature,
        // Left out when none, not for older peers: a server and a device keep
        // these as JSON, and older records have no key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root: Option<RootKey>,
        /// The identity's genesis, for a verifier that has not seen it
        /// (as [`UserDeviceCredential::genesis`]).
        // Left out when none, not for older peers: a server and a device keep
        // these as JSON, and older records have no key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        genesis: Option<Box<IdentityGenesis>>,
    },
    /// `credential`'s device key's signature.
    Device {
        credential: UserDeviceCredential,
        #[serde(with = "signature_hex")]
        signature: Signature,
    },
}

/// Why a [`TieProof`] does not prove its user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TieProofError {
    /// The key it names did not sign this message: made for another
    /// server, nonce, identity or account, or not signed at all.
    #[error("the tie is not signed for this server, challenge, identity and account")]
    BadSignature,
    /// A device credential of another identity than the one to tie.
    #[error("the device credential is another identity's")]
    OtherIdentity,
    /// The device credential does not hold.
    #[error(transparent)]
    Credential(#[from] CredentialError),
}

impl TieProof {
    /// Whether it proves `user` signed `message` (a [`tie_message`]) at
    /// `now` (Unix seconds): the signature of the root it names, or a
    /// device key's with a credential for `user` that holds now. Whether
    /// that root is one of the user's ([`Self::root`]) and revocation are
    /// the verifier's to check, against every state it has seen.
    pub fn verify(
        &self,
        user: &PortableUserId,
        message: &[u8],
        now: i64,
    ) -> Result<(), TieProofError> {
        match self {
            TieProof::Root {
                signature, root, ..
            } => root
                .clone()
                .unwrap_or_else(|| RootKey::phrase_of(user))
                .verify(message, signature)
                .then_some(())
                .ok_or(TieProofError::BadSignature),
            TieProof::Device {
                credential,
                signature,
            } => {
                if credential.user != *user {
                    return Err(TieProofError::OtherIdentity);
                }
                credential.verify(now)?;
                credential
                    .device_key
                    .verify(message, signature)
                    .then_some(())
                    .ok_or(TieProofError::BadSignature)
            }
        }
    }

    /// The root whose word it is: the one that signed it, or the one that
    /// signed the device's credential.
    pub fn root(&self, user: &PortableUserId) -> RootKey {
        match self {
            TieProof::Root { root, .. } => root.clone().unwrap_or_else(|| RootKey::phrase_of(user)),
            TieProof::Device { credential, .. } => credential.signer(),
        }
    }

    /// The identity's genesis it carries, if any.
    pub fn genesis(&self) -> Option<&IdentityGenesis> {
        match self {
            TieProof::Root { genesis, .. } => genesis.as_deref(),
            TieProof::Device { credential, .. } => credential.genesis.as_deref(),
        }
    }

    /// What kind of proof it is, as the server keeps it: `root` or
    /// `device`.
    pub fn kind(&self) -> &'static str {
        match self {
            TieProof::Root { .. } => "root",
            TieProof::Device { .. } => "device",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7; 32])
    }

    fn id() -> PortableUserId {
        PortableUserId::from_bytes(key().verifying_key().to_bytes()).unwrap()
    }

    #[test]
    fn the_text_form_is_lowercase_hex_and_reads_back() {
        let text = id().to_string();
        assert_eq!(text.len(), 64);
        assert!(text.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        assert_eq!(text.parse::<PortableUserId>(), Ok(id()));
        assert_eq!(text.to_uppercase().parse::<PortableUserId>(), Ok(id()));
    }

    #[test]
    fn serde_uses_the_text_form() {
        let json = serde_json::to_string(&id()).unwrap();
        assert_eq!(json, format!("\"{}\"", id()));
        assert_eq!(serde_json::from_str::<PortableUserId>(&json).unwrap(), id());
    }

    #[test]
    fn text_that_is_not_an_id_is_refused() {
        assert!("abc".parse::<PortableUserId>().is_err());
        assert!("zz".repeat(32).parse::<PortableUserId>().is_err());
        assert!("é".repeat(32).parse::<PortableUserId>().is_err());
        assert!(serde_json::from_str::<PortableUserId>("\"abc\"").is_err());
    }

    #[test]
    fn an_id_that_is_not_a_curve_point_is_an_id_no_phrase_root_signs_for() {
        // An id derived from a genesis (P7-08) is a hash, half of which
        // have no point on the curve: such an id reads, but no phrase
        // root's signature passes for it.
        let point = (0..=255u8)
            .map(|y| {
                let mut bytes = [0u8; 32];
                bytes[0] = y;
                bytes
            })
            .find(|bytes| VerifyingKey::from_bytes(bytes).is_err())
            .unwrap();
        let id = PortableUserId::from_bytes(point).unwrap();
        assert_eq!(id.to_string().parse::<PortableUserId>(), Ok(id));
        assert!(!id.verify(b"anything", &key().sign(b"anything")));
    }

    #[test]
    fn the_fingerprint_is_the_server_format() {
        let code = id().fingerprint();
        assert_eq!(code, fingerprint::fingerprint(id().as_bytes()));
        assert_eq!(code.len(), 19);
    }

    #[test]
    fn verify_accepts_the_roots_signature_and_nothing_else() {
        let signature = key().sign(b"device credential");
        assert!(id().verify(b"device credential", &signature));
        assert!(!id().verify(b"another message", &signature));
        let other = SigningKey::from_bytes(&[8; 32]).sign(b"device credential");
        assert!(!id().verify(b"device credential", &other));
    }

    // P7-02. The vectors below were computed apart from this code: the
    // messages laid out by hand in Python's `struct`, the keys and
    // signatures by `openssl pkey` and `openssl pkeyutl -sign -rawin` over
    // the raw seeds. Ed25519 signatures are deterministic, so the same
    // seed and message give these bytes anywhere.

    /// 2026-10-09T00:00:00Z.
    const T: i64 = 1_791_504_000;

    const ROOT_7: &str = "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const DEVICE_9: &str = "fd1724385aa0c75b64fb78cd602fa1d991fdebf76b13c58ed702eac835e9f618";
    const DEVICE_11: &str = "66be7e332c7a453332bd9d0a7f7db055f5c5ef1a06ada66d98b39fb6810c473a";

    /// Root 7's credential for device 9: serial 3, issued at [`T`], for
    /// 90 days.
    const CREDENTIAL_MESSAGE: &str = "73706f6f6c6672616d65206465766963652063726564656e7469616c20763100\
        ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c\
        fd1724385aa0c75b64fb78cd602fa1d991fdebf76b13c58ed702eac835e9f618\
        0000000000000003000000006ac82e80000000006b3ed580";
    const CREDENTIAL_SIGNATURE: &str = "91130d366819d59e169d9d744145be3b13bd87badee3f71cb6e4d01007735939\
        516ddd13f9bd54ac37c9ab667a5e5c09970248113697abcf166dcb6838351308";

    /// Root 7's state at revision 2, issued at [`T`], revoking device 9 a
    /// day before and device 11 an hour before (sorted by key: 11's is the
    /// lower).
    const STATE_MESSAGE: &str = "73706f6f6c6672616d65206964656e7469747920737461746520763200\
        ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c\
        0000000000000002000000006ac82e8000000002\
        66be7e332c7a453332bd9d0a7f7db055f5c5ef1a06ada66d98b39fb6810c473a000000006ac82070\
        fd1724385aa0c75b64fb78cd602fa1d991fdebf76b13c58ed702eac835e9f618000000006ac6dd00";
    const STATE_SIGNATURE: &str = "01853dcafaade8a6780eb0b007ee7a89e79c3d2a21603d3a0231e794b5477901\
        03b097d7c5b74e466158c2ead0d1b5b8cadf2a4d95196a5651d9fb3cef59da0d";

    /// Device 9's proof to server `[0x5a; 32]` over nonce `[0xa5; 32]`.
    const PROOF_MESSAGE: &str = "73706f6f6c6672616d65206465766963652070726f6f6620763100\
        5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a\
        a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5";
    const PROOF_SIGNATURE: &str = "712ca0bb7456d37e02be5b5f24ef61371d864e3cd5d81b0d306df1efea87037e\
        3dee370b29920ecd75ba6a72fdfe67bbf9a7bb3609da0ac97c7b1ed29260b503";

    const SERVER: [u8; 32] = [0x5a; 32];
    const NONCE: [u8; 32] = [0xa5; 32];

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn signature(text: &str) -> Signature {
        Signature::from_bytes(&parse_hex(text).unwrap())
    }

    fn root() -> SigningKey {
        SigningKey::from_bytes(&[7; 32])
    }

    fn device_secret(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn device(seed: u8) -> DevicePublicKey {
        DevicePublicKey::from_bytes(device_secret(seed).verifying_key().to_bytes()).unwrap()
    }

    fn credential_from(
        root: &SigningKey,
        user: PortableUserId,
        issued_at: i64,
        expires_at: i64,
    ) -> UserDeviceCredential {
        UserDeviceCredential::new(user, device(9), 3, issued_at, expires_at, |m| root.sign(m))
    }

    fn credential() -> UserDeviceCredential {
        credential_from(&root(), id(), T, T + MAX_CREDENTIAL_VALIDITY)
    }

    /// Device `seed`'s key, revoked at `revoked_at`.
    fn revoked(seed: u8, revoked_at: i64) -> RevokedKey {
        RevokedKey {
            key: device(seed),
            revoked_at,
        }
    }

    /// The vectors' entries: device 9 a day before [`T`], 11 an hour.
    fn nine() -> RevokedKey {
        revoked(9, T - DAY)
    }

    fn eleven() -> RevokedKey {
        revoked(11, T - 3600)
    }

    fn state(revision: u64, revoked: Vec<RevokedKey>) -> PortableIdentityState {
        PortableIdentityState::new(id(), revision, revoked, T, |m| root().sign(m)).unwrap()
    }

    /// A state at `revision` that removes key `seed`'s root at `at`.
    fn removing(revision: u64, seed: u8, at: i64) -> PortableIdentityState {
        PortableIdentityState::signed_by(
            id(),
            RootKey::phrase_of(&id()),
            revision,
            StateChanges {
                roots_removed: vec![RemovedRoot {
                    root: soft_key::SoftKey::from_seed(seed).root(),
                    removed_at: at,
                }],
                ..StateChanges::default()
            },
            T,
            |m| root().sign(m).into(),
        )
        .unwrap()
    }

    /// A rival's removal is covered only by a removal of the same root, no
    /// later: a later one would move the time root additions are judged by.
    #[test]
    fn a_removal_is_covered_by_the_same_removal_no_later() {
        let removal = removing(2, 3, T);
        assert!(!removal.root_changes_covered_by(&[]));
        assert!(!removal.root_changes_covered_by(&[state(5, vec![nine()])]));
        assert!(!removal.root_changes_covered_by(&[removing(5, 4, T)]));
        assert!(!removal.root_changes_covered_by(&[removing(5, 3, T + 1)]));
        assert!(removal.root_changes_covered_by(&[removing(5, 3, T)]));
        assert!(removal.root_changes_covered_by(&[state(5, vec![]), removing(1, 3, T - 1)]));
        // Revocations alone change no roots: covered by anything.
        assert!(state(2, vec![nine()]).root_changes_covered_by(&[]));
    }

    /// Of two states at one revision, every verifier takes the same one
    /// as the newest, whichever it saw first.
    #[test]
    fn rivals_at_one_revision_are_ordered_the_same_everywhere() {
        let (one, other) = (state(3, vec![nine()]), state(3, vec![eleven()]));
        assert_ne!(one.digest(), other.digest());
        let mut seen = [one.clone(), other.clone()];
        seen.sort_by(newest_first);
        let mut reversed = [other, one];
        reversed.sort_by(newest_first);
        assert_eq!(seen, reversed);
        let mut by_revision = [state(2, vec![]), state(4, vec![])];
        by_revision.sort_by(newest_first);
        assert_eq!(by_revision[0].revision, 4);
    }

    #[test]
    fn the_keys_are_the_vectors() {
        assert_eq!(id().to_string(), ROOT_7);
        assert_eq!(device(9).to_string(), DEVICE_9);
        assert_eq!(device(11).to_string(), DEVICE_11);
    }

    #[test]
    fn the_credential_message_is_the_fixed_layout() {
        let credential = credential();
        let message = UserDeviceCredential::message(
            &credential.user,
            &credential.device_key,
            credential.serial,
            credential.issued_at,
            credential.expires_at,
        );
        assert_eq!(hex(&message), CREDENTIAL_MESSAGE);
        assert_eq!(
            hex(&credential.signature.as_ed25519().unwrap().to_bytes()),
            CREDENTIAL_SIGNATURE
        );
    }

    #[test]
    fn a_credential_signed_elsewhere_verifies() {
        let credential = UserDeviceCredential {
            signature: signature(CREDENTIAL_SIGNATURE).into(),
            ..credential()
        };
        assert_eq!(credential.verify(T), Ok(()));
    }

    #[test]
    fn the_state_message_is_the_fixed_layout_whatever_the_order() {
        // Given unsorted and with a repeat, it signs the sorted set, the
        // repeat at its earliest.
        let state = state(2, vec![revoked(9, T), eleven(), nine()]);
        assert_eq!(state.revoked, [eleven(), nine()]);
        let message = PortableIdentityState::message(&state.user, 2, &state.revoked, T);
        assert_eq!(hex(&message), STATE_MESSAGE);
        assert_eq!(
            hex(&state.signature.as_ed25519().unwrap().to_bytes()),
            STATE_SIGNATURE
        );
        let reordered = PortableIdentityState {
            revoked: vec![nine(), eleven(), revoked(9, T)],
            signature: signature(STATE_SIGNATURE).into(),
            ..state.clone()
        };
        assert_eq!(reordered.verify(), Ok(()));
        assert!(reordered.same_as(&state));
    }

    #[test]
    fn the_proof_message_is_the_fixed_layout() {
        let message = device_proof_message(&SERVER, &NONCE);
        assert_eq!(hex(&message), PROOF_MESSAGE);
        assert_eq!(
            verify_device_proof(&credential(), &SERVER, &NONCE, &signature(PROOF_SIGNATURE)),
            Ok(())
        );
    }

    #[test]
    fn serde_pins_the_credential() {
        let json = serde_json::to_string(&credential()).unwrap();
        assert_eq!(
            json,
            format!(
                "{{\"user\":\"{ROOT_7}\",\"device_key\":\"{DEVICE_9}\",\"serial\":3,\
                 \"issued_at\":{T},\"expires_at\":{},\"signature\":\"{CREDENTIAL_SIGNATURE}\"}}",
                T + MAX_CREDENTIAL_VALIDITY
            )
        );
        let back: UserDeviceCredential = serde_json::from_str(&json).unwrap();
        assert_eq!(back, credential());
    }

    #[test]
    fn serde_pins_the_state() {
        let json = serde_json::to_string(&state(2, vec![nine(), eleven()])).unwrap();
        assert_eq!(
            json,
            format!(
                "{{\"user\":\"{ROOT_7}\",\"revision\":2,\"revoked\":[\
                 {{\"key\":\"{DEVICE_11}\",\"revoked_at\":{}}},\
                 {{\"key\":\"{DEVICE_9}\",\"revoked_at\":{}}}],\
                 \"issued_at\":{T},\"signature\":\"{STATE_SIGNATURE}\"}}",
                T - 3600,
                T - DAY
            )
        );
        let back: PortableIdentityState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.verify(), Ok(()));
    }

    #[test]
    fn a_signature_that_is_not_hex_is_refused() {
        let json = serde_json::to_string(&credential()).unwrap();
        let short = json.replace(CREDENTIAL_SIGNATURE, "abcd");
        assert!(serde_json::from_str::<UserDeviceCredential>(&short).is_err());
    }

    #[test]
    fn a_device_key_that_is_not_a_curve_point_is_refused() {
        let point = (0..=255u8)
            .map(|y| {
                let mut bytes = [0u8; 32];
                bytes[0] = y;
                bytes
            })
            .find(|bytes| VerifyingKey::from_bytes(bytes).is_err())
            .unwrap();
        assert!(DevicePublicKey::from_bytes(point).is_err());
        assert!(hex(&point).parse::<DevicePublicKey>().is_err());
        assert!("abc".parse::<DevicePublicKey>().is_err());
        assert_eq!(DEVICE_9.parse::<DevicePublicKey>(), Ok(device(9)));
    }

    #[test]
    fn a_credential_is_good_from_issue_to_expiry() {
        let credential = credential();
        assert_eq!(credential.verify(T), Ok(()));
        assert_eq!(credential.verify(T + MAX_CREDENTIAL_VALIDITY - 1), Ok(()));
        assert_eq!(
            credential.verify(T + MAX_CREDENTIAL_VALIDITY),
            Err(CredentialError::Expired)
        );
    }

    #[test]
    fn a_credential_from_the_future_is_good_within_the_skew_only() {
        let credential = credential();
        assert_eq!(credential.verify(T - CLOCK_SKEW), Ok(()));
        assert_eq!(
            credential.verify(T - CLOCK_SKEW - 1),
            Err(CredentialError::NotYetValid)
        );
    }

    #[test]
    fn a_credential_longer_than_ninety_days_is_refused() {
        let long = credential_from(&root(), id(), T, T + MAX_CREDENTIAL_VALIDITY + 1);
        assert_eq!(long.verify(T), Err(CredentialError::TooLong));
        let backwards = credential_from(&root(), id(), i64::MIN, i64::MAX);
        assert_eq!(backwards.verify(T), Err(CredentialError::TooLong));
    }

    #[test]
    fn a_tampered_credential_is_refused() {
        let good = credential();
        let tampered = [
            UserDeviceCredential {
                device_key: device(11),
                ..good.clone()
            },
            UserDeviceCredential {
                serial: 4,
                ..good.clone()
            },
            UserDeviceCredential {
                issued_at: T - 1,
                ..good.clone()
            },
            UserDeviceCredential {
                expires_at: T + 1,
                ..good.clone()
            },
        ];
        for credential in tampered {
            assert_eq!(
                credential.verify(T),
                Err(CredentialError::BadSignature),
                "{credential:?}"
            );
        }
    }

    #[test]
    fn a_credential_another_root_signed_is_refused() {
        // Root 8 signs a credential naming root 7's user.
        let forged = credential_from(&SigningKey::from_bytes(&[8; 32]), id(), T, T + DAY);
        assert_eq!(forged.verify(T), Err(CredentialError::BadSignature));
    }

    #[test]
    fn a_proof_by_another_key_or_for_another_server_is_refused() {
        let credential = credential();
        let by_other = device_secret(11).sign(&device_proof_message(&SERVER, &NONCE));
        assert_eq!(
            verify_device_proof(&credential, &SERVER, &NONCE, &by_other),
            Err(CredentialError::BadDeviceProof)
        );
        let good = device_secret(9).sign(&device_proof_message(&SERVER, &NONCE));
        assert_eq!(
            verify_device_proof(&credential, &[0; 32], &NONCE, &good),
            Err(CredentialError::BadDeviceProof)
        );
        assert_eq!(
            verify_device_proof(&credential, &SERVER, &[0; 32], &good),
            Err(CredentialError::BadDeviceProof)
        );
        assert_eq!(
            verify_device_proof(&credential, &SERVER, &NONCE, &good),
            Ok(())
        );
    }

    #[test]
    fn a_state_revokes_what_it_lists() {
        let state = state(2, vec![nine()]);
        assert_eq!(state.verify(), Ok(()));
        assert!(state.revokes(&device(9)));
        assert!(!state.revokes(&device(11)));
    }

    #[test]
    fn a_tampered_state_is_refused() {
        let good = state(2, vec![nine(), eleven()]);
        let tampered = [
            PortableIdentityState {
                revision: 3,
                ..good.clone()
            },
            PortableIdentityState {
                revoked: vec![nine()],
                ..good.clone()
            },
            PortableIdentityState {
                revoked: vec![nine(), revoked(11, T)],
                ..good.clone()
            },
            PortableIdentityState {
                issued_at: T + 1,
                ..good.clone()
            },
        ];
        for state in tampered {
            assert_eq!(state.verify(), Err(StateError::BadSignature), "{state:?}");
        }
        let by_other = PortableIdentityState::new(id(), 2, vec![], T, |m| {
            SigningKey::from_bytes(&[8; 32]).sign(m)
        })
        .unwrap();
        assert_eq!(by_other.verify(), Err(StateError::BadSignature));
    }

    #[test]
    fn a_state_revoking_more_than_the_limit_is_refused() {
        let keys: Vec<_> = (0u16..)
            .map(|n| SigningKey::from_bytes(&blake3::hash(&n.to_be_bytes()).into()))
            .map(|key| RevokedKey {
                key: DevicePublicKey::from_bytes(key.verifying_key().to_bytes()).unwrap(),
                revoked_at: T,
            })
            .take(MAX_REVOKED + 1)
            .collect();
        let sign = |m: &[u8]| root().sign(m);
        assert!(PortableIdentityState::new(id(), 1, keys[..MAX_REVOKED].to_vec(), T, sign).is_ok());
        assert_eq!(
            PortableIdentityState::new(id(), 1, keys.clone(), T, sign).unwrap_err(),
            StateError::TooManyRevoked
        );
        // Signed over the full list by a root that ignored the limit.
        let oversized = PortableIdentityState {
            user: id(),
            revision: 1,
            signature: root()
                .sign(&PortableIdentityState::message(&id(), 1, &keys, T))
                .into(),
            revoked: keys,
            issued_at: T,
            signer: None,
            roots_added: vec![],
            roots_removed: vec![],
        };
        assert_eq!(oversized.verify(), Err(StateError::TooManyRevoked));
    }

    #[test]
    fn a_revocation_lapses_once_no_credential_it_could_stop_holds() {
        // A credential issued at the revocation, up to the skew ahead,
        // holds until just before this.
        let issued_at = T + CLOCK_SKEW;
        let last_good = issued_at + MAX_CREDENTIAL_VALIDITY - 1;
        assert!(!revocation_lapsed(T, last_good));
        assert!(!revocation_lapsed(T, T + REVOCATION_WINDOW));
        assert!(revocation_lapsed(T, T + REVOCATION_WINDOW + 1));
        assert!(!revocation_lapsed(i64::MIN, i64::MIN));
    }

    // P7-04. Computed apart from this code as above: the message laid out
    // in Python's `struct` (`>H` for the length), the signatures by
    // `openssl pkeyutl -sign -rawin` with the raw seeds 7 (the root) and 9
    // (a device).

    /// Root 7 tying to account `alice` on server `[0x5a; 32]`, over nonce
    /// `[0xa5; 32]`.
    const TIE_MESSAGE: &str = "73706f6f6c6672616d6520746965207631\
        00\
        5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a\
        a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5\
        ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c\
        0005616c696365";
    const TIE_ROOT_SIGNATURE: &str = "030766ca4d18dba1d29c76d341775a797460d33fd78a0c091ecf09ca4605e59f\
        6a4d1504ecbc680e1b41ec9df7c7cfa135ff4afc823fcd25f3761dc2fff7ef02";
    const TIE_DEVICE_SIGNATURE: &str = "08a319d99188a15d34259fc193ad36a3024c67e98b336644c362014052b20e85\
        0564e5d631d13b20d2212134e7f4a66da90d432c28a043b2819be45029e8f003";

    fn tie(username: &str) -> Vec<u8> {
        tie_message(&SERVER, &NONCE, &id(), username).unwrap()
    }

    #[test]
    fn the_tie_message_is_the_fixed_layout() {
        assert_eq!(hex(&tie("alice")), TIE_MESSAGE);
        assert_eq!(
            hex(&root().sign(&tie("alice")).to_bytes()),
            TIE_ROOT_SIGNATURE
        );
    }

    #[test]
    fn a_tie_signed_elsewhere_verifies_by_root_or_device() {
        let by_root = TieProof::Root {
            signature: signature(TIE_ROOT_SIGNATURE).into(),
            root: None,
            genesis: None,
        };
        assert_eq!(by_root.verify(&id(), &tie("alice"), T), Ok(()));
        let by_device = TieProof::Device {
            credential: credential(),
            signature: signature(TIE_DEVICE_SIGNATURE),
        };
        assert_eq!(by_device.verify(&id(), &tie("alice"), T), Ok(()));
    }

    #[test]
    fn a_tie_for_another_server_nonce_identity_or_account_is_refused() {
        let by_root = TieProof::Root {
            signature: signature(TIE_ROOT_SIGNATURE).into(),
            root: None,
            genesis: None,
        };
        let other_user =
            PortableUserId::from_bytes(SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes())
                .unwrap();
        let elsewhere = [
            tie_message(&[0; 32], &NONCE, &id(), "alice").unwrap(),
            tie_message(&SERVER, &[0; 32], &id(), "alice").unwrap(),
            tie_message(&SERVER, &NONCE, &other_user, "alice").unwrap(),
            tie("bob"),
            // A name that starts the same is another account.
            tie("alicea"),
        ];
        for message in elsewhere {
            assert_eq!(
                by_root.verify(&id(), &message, T),
                Err(TieProofError::BadSignature)
            );
        }
        // Signed by the other identity's root for itself: not root 7's.
        assert_eq!(
            by_root.verify(&other_user, &tie("alice"), T),
            Err(TieProofError::BadSignature)
        );
    }

    #[test]
    fn a_device_proof_holds_only_with_a_good_credential_for_the_user() {
        let signature = device_secret(9).sign(&tie("alice"));
        let expired = TieProof::Device {
            credential: credential(),
            signature,
        };
        assert_eq!(
            expired.verify(&id(), &tie("alice"), T + MAX_CREDENTIAL_VALIDITY),
            Err(TieProofError::Credential(CredentialError::Expired))
        );
        // Device 11 signs, with device 9's credential.
        let wrong_key = TieProof::Device {
            credential: credential(),
            signature: device_secret(11).sign(&tie("alice")),
        };
        assert_eq!(
            wrong_key.verify(&id(), &tie("alice"), T),
            Err(TieProofError::BadSignature)
        );
        let other_root = SigningKey::from_bytes(&[8; 32]);
        let other_user = PortableUserId::from_bytes(other_root.verifying_key().to_bytes()).unwrap();
        let theirs = TieProof::Device {
            credential: credential_from(&other_root, other_user, T, T + DAY),
            signature,
        };
        assert_eq!(
            theirs.verify(&id(), &tie("alice"), T),
            Err(TieProofError::OtherIdentity)
        );
    }

    #[test]
    fn serde_pins_the_tie_proof() {
        let json = serde_json::to_value(TieProof::Root {
            signature: signature(TIE_ROOT_SIGNATURE).into(),
            root: None,
            genesis: None,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type": "root", "signature": TIE_ROOT_SIGNATURE})
        );
        let device = TieProof::Device {
            credential: credential(),
            signature: signature(TIE_DEVICE_SIGNATURE),
        };
        let back: TieProof =
            serde_json::from_value(serde_json::to_value(&device).unwrap()).unwrap();
        assert_eq!(back, device);
        assert_eq!(device.kind(), "device");
    }

    #[test]
    fn a_username_too_long_to_count_has_no_tie_message() {
        assert!(tie_message(&SERVER, &NONCE, &id(), &"a".repeat(65_536)).is_none());
        assert!(tie_message(&SERVER, &NONCE, &id(), &"a".repeat(65_535)).is_some());
    }
}
