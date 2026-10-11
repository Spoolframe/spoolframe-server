//! An identity's roots (P7-08): the keys that may act for it. A phrase
//! root is P7-01's Ed25519 key; a security key root is a FIDO2 key's
//! P-256 credential, which signs only CTAP2 assertions. An identity has
//! one or more, any one of which is enough, and its genesis
//! ([`IdentityGenesis`]) names the first.
//!
//! What a root signs keeps the layouts of P7-02: a domain string with its
//! NUL, then fixed-width fields. A phrase root signs those bytes; a
//! security key is given their SHA-256 as `clientDataHash`, as raw CTAP2
//! lets the client choose, and signs `authenticatorData ‖ clientDataHash`
//! under [`PORTABLE_RP`].

use std::fmt;

use ed25519_dalek::VerifyingKey;
use p256::ecdsa::signature::Verifier;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use super::{PortableUserId, Signature, parse_hex, write_hex};

/// The relying party a security key root signs under. Domain-shaped and
/// under PX-14's `*.spoolframe.app`, so no server's passkey sign-in
/// (under that server's own RP) can pass as a root's signature.
pub const PORTABLE_RP: &str = "portable.spoolframe.app";

/// The most roots an identity has at once, and in its genesis.
pub const MAX_ROOTS: usize = 8;

/// The most root changes a state lists: roots added, and roots removed,
/// each. Changes are cumulative, as revocations are, so a state carries
/// them all.
pub const MAX_ROOT_CHANGES: usize = 64;

/// The longest credential id a security key root may have, in bytes.
pub const MAX_CREDENTIAL_ID: usize = 128;

/// The longest `authenticatorData` a root signature may carry. An
/// assertion's is 37 bytes and its extensions; this leaves room for them.
pub const MAX_AUTHENTICATOR_DATA: usize = 512;

/// The longest DER ECDSA P-256 signature.
pub const MAX_DER_SIGNATURE: usize = 72;

/// Bytes in a P-256 public key, SEC1 uncompressed: `04 ‖ x ‖ y`.
pub const P256_PUBLIC_KEY_LEN: usize = 65;

/// The domain of each root's signature over its identity's genesis.
pub const GENESIS_DOMAIN: &[u8] = b"spoolframe identity genesis v1\0";

/// The domain of an added root's proof that it is held, over its
/// addition.
pub const ROOT_ADD_DOMAIN: &[u8] = b"spoolframe root add v1\0";

/// The domain of the adding root's endorsement of a root it adds.
pub const ROOT_ENDORSE_DOMAIN: &[u8] = b"spoolframe root endorse v1\0";

/// The context the id of an identity with a security key root is derived
/// in, from its genesis.
pub const USER_ID_CONTEXT: &str = "spoolframe portable user id v1";

/// `authenticatorData` flag: the user was present (touched the key).
const FLAG_UP: u8 = 0x01;
/// `authenticatorData` flag: the user was verified (gave the PIN).
const FLAG_UV: u8 = 0x04;

/// Bytes of `authenticatorData` before its flags: the RP id hash.
const RP_ID_HASH_LEN: usize = 32;
/// The shortest `authenticatorData`: RP id hash, flags, counter.
const MIN_AUTHENTICATOR_DATA: usize = RP_ID_HASH_LEN + 1 + 4;

/// SHA-256 of [`PORTABLE_RP`]: what an assertion's `authenticatorData`
/// starts with.
pub fn portable_rp_id_hash() -> [u8; 32] {
    Sha256::digest(PORTABLE_RP.as_bytes()).into()
}

/// A key that may act for an identity.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RootKey {
    /// P7-01's root: an Ed25519 key derived from a recovery phrase.
    Phrase([u8; 32]),
    /// A FIDO2 security key's credential: its P-256 public key (SEC1
    /// uncompressed) and its credential id, at most
    /// [`MAX_CREDENTIAL_ID`] bytes.
    SecurityKey {
        public_key: [u8; P256_PUBLIC_KEY_LEN],
        credential_id: Vec<u8>,
    },
}

impl RootKey {
    /// The phrase root of a phrase identity: the Ed25519 key its id is.
    pub fn phrase_of(user: &PortableUserId) -> Self {
        RootKey::Phrase(*user.as_bytes())
    }

    /// How a root is laid out in what roots sign: `01 ‖ key (32)` for a
    /// phrase root; `02 ‖ public key (65) ‖ id length (u8) ‖ id` for a
    /// security key. Each layout says its own length.
    pub fn bytes(&self) -> Vec<u8> {
        match self {
            RootKey::Phrase(key) => {
                let mut bytes = Vec::with_capacity(33);
                bytes.push(1);
                bytes.extend_from_slice(key);
                bytes
            }
            RootKey::SecurityKey {
                public_key,
                credential_id,
            } => {
                let mut bytes = Vec::with_capacity(1 + 65 + 1 + credential_id.len());
                bytes.push(2);
                bytes.extend_from_slice(public_key);
                bytes.push(u8::try_from(credential_id.len()).expect("a credential id fits a byte"));
                bytes.extend_from_slice(credential_id);
                bytes
            }
        }
    }

    /// Whether it is a key a root could be: an Ed25519 point, or a P-256
    /// point with a credential id within [`MAX_CREDENTIAL_ID`].
    pub fn is_valid(&self) -> bool {
        match self {
            RootKey::Phrase(key) => VerifyingKey::from_bytes(key).is_ok(),
            RootKey::SecurityKey {
                public_key,
                credential_id,
            } => {
                !credential_id.is_empty()
                    && credential_id.len() <= MAX_CREDENTIAL_ID
                    && p256::ecdsa::VerifyingKey::from_sec1_bytes(public_key).is_ok()
            }
        }
    }

    /// Whether `signature` is this root's over `message`: a strict
    /// Ed25519 signature from a phrase root; a CTAP2 assertion from a
    /// security key, under [`PORTABLE_RP`], with the user present and
    /// verified, over `authenticatorData ‖ SHA-256(message)`. The
    /// signature counter is not looked at: a key may report 0, and a
    /// cloned key is not what this guards against.
    pub fn verify(&self, message: &[u8], signature: &RootSignature) -> bool {
        match (self, signature) {
            (RootKey::Phrase(key), RootSignature::Ed25519(signature)) => {
                VerifyingKey::from_bytes(key)
                    .is_ok_and(|key| key.verify_strict(message, signature).is_ok())
            }
            (
                RootKey::SecurityKey { public_key, .. },
                RootSignature::Ctap {
                    authenticator_data,
                    signature,
                },
            ) => verify_ctap(public_key, message, authenticator_data, signature),
            _ => false,
        }
    }

    /// The short code a person compares between screens, in an
    /// identity's format ([`crate::fingerprint`]): of the phrase root's
    /// key, or of the BLAKE3 of a security key's [`Self::bytes`].
    pub fn fingerprint(&self) -> String {
        match self {
            RootKey::Phrase(key) => crate::fingerprint::fingerprint(key),
            RootKey::SecurityKey { .. } => {
                crate::fingerprint::fingerprint(blake3::hash(&self.bytes()).as_bytes())
            }
        }
    }

    /// What kind of root it is: `phrase` or `security_key`.
    pub fn kind(&self) -> &'static str {
        match self {
            RootKey::Phrase(_) => "phrase",
            RootKey::SecurityKey { .. } => "security_key",
        }
    }
}

impl fmt::Debug for RootKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RootKey({})", self.fingerprint())
    }
}

/// Whether the CTAP2 assertion `authenticator_data`, `der` is
/// `public_key`'s over `message` (see [`RootKey::verify`]).
fn verify_ctap(
    public_key: &[u8; P256_PUBLIC_KEY_LEN],
    message: &[u8],
    authenticator_data: &[u8],
    der: &[u8],
) -> bool {
    if authenticator_data.len() < MIN_AUTHENTICATOR_DATA
        || authenticator_data.len() > MAX_AUTHENTICATOR_DATA
        || der.len() > MAX_DER_SIGNATURE
    {
        return false;
    }
    if authenticator_data[..RP_ID_HASH_LEN] != portable_rp_id_hash() {
        return false;
    }
    let flags = authenticator_data[RP_ID_HASH_LEN];
    if flags & FLAG_UP == 0 || flags & FLAG_UV == 0 {
        return false;
    }
    let (Ok(key), Ok(signature)) = (
        p256::ecdsa::VerifyingKey::from_sec1_bytes(public_key),
        p256::ecdsa::Signature::from_der(der),
    ) else {
        return false;
    };
    let mut signed = Vec::with_capacity(authenticator_data.len() + 32);
    signed.extend_from_slice(authenticator_data);
    signed.extend_from_slice(&Sha256::digest(message));
    key.verify(&signed, &signature).is_ok()
}

/// A root's signature: a phrase root's Ed25519 one, or a security key's
/// CTAP2 assertion (its `authenticatorData` and DER ECDSA signature).
///
/// In serde, an Ed25519 signature is 128 hex digits, as P7-02 wrote it,
/// so a phrase root's credential and state read as they always did; an
/// assertion is `{"authenticator_data": hex, "signature": hex}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootSignature {
    Ed25519(Signature),
    Ctap {
        authenticator_data: Vec<u8>,
        signature: Vec<u8>,
    },
}

impl RootSignature {
    /// The Ed25519 signature, for a phrase root's.
    pub fn as_ed25519(&self) -> Option<&Signature> {
        match self {
            RootSignature::Ed25519(signature) => Some(signature),
            RootSignature::Ctap { .. } => None,
        }
    }
}

impl From<Signature> for RootSignature {
    fn from(signature: Signature) -> Self {
        RootSignature::Ed25519(signature)
    }
}

/// Hex for serde, over a byte string of at most `max` bytes.
pub(crate) mod hex_vec {
    use super::*;

    struct Hex<'a>(&'a [u8]);

    impl fmt::Display for Hex<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write_hex(f, self.0)
        }
    }

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&Hex(bytes))
    }

    /// Even-length hex, either case, of at most `max` bytes.
    pub fn parse(text: &str, max: usize) -> Option<Vec<u8>> {
        if !text.len().is_multiple_of(2) || text.len() > 2 * max || !text.is_ascii() {
            return None;
        }
        (0..text.len() / 2)
            .map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok())
            .collect()
    }
}

#[derive(Serialize, Deserialize)]
struct CtapWire {
    #[serde(serialize_with = "hex_vec::serialize")]
    #[serde(deserialize_with = "authenticator_data_hex")]
    authenticator_data: Vec<u8>,
    #[serde(serialize_with = "hex_vec::serialize")]
    #[serde(deserialize_with = "der_hex")]
    signature: Vec<u8>,
}

fn authenticator_data_hex<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let text = String::deserialize(d)?;
    hex_vec::parse(&text, MAX_AUTHENTICATOR_DATA)
        .ok_or_else(|| serde::de::Error::custom("authenticator data is at most 512 bytes of hex"))
}

fn der_hex<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let text = String::deserialize(d)?;
    hex_vec::parse(&text, MAX_DER_SIGNATURE)
        .ok_or_else(|| serde::de::Error::custom("a DER signature is at most 72 bytes of hex"))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RootSignatureWire {
    Ed25519(String),
    Ctap(CtapWire),
}

impl Serialize for RootSignature {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            RootSignature::Ed25519(signature) => super::signature_hex::serialize(signature, s),
            RootSignature::Ctap {
                authenticator_data,
                signature,
            } => CtapWire {
                authenticator_data: authenticator_data.clone(),
                signature: signature.clone(),
            }
            .serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for RootSignature {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match RootSignatureWire::deserialize(d)? {
            RootSignatureWire::Ed25519(text) => parse_hex::<{ Signature::BYTE_SIZE }>(&text)
                .map(|bytes| RootSignature::Ed25519(Signature::from_bytes(&bytes)))
                .ok_or_else(|| serde::de::Error::custom("a signature is 128 hex digits")),
            RootSignatureWire::Ctap(CtapWire {
                authenticator_data,
                signature,
            }) => Ok(RootSignature::Ctap {
                authenticator_data,
                signature,
            }),
        }
    }
}

/// A root in serde: `{"type": "phrase", "public_key": hex}` or
/// `{"type": "security_key", "public_key": hex, "credential_id": hex}`.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RootKeyWire {
    Phrase {
        public_key: String,
    },
    SecurityKey {
        public_key: String,
        credential_id: String,
    },
}

impl Serialize for RootKey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        match self {
            RootKey::Phrase(key) => RootKeyWire::Phrase {
                public_key: hex(key),
            },
            RootKey::SecurityKey {
                public_key,
                credential_id,
            } => RootKeyWire::SecurityKey {
                public_key: hex(public_key),
                credential_id: hex(credential_id),
            },
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for RootKey {
    /// Sizes are checked here; whether the keys are points is
    /// [`RootKey::is_valid`]'s, asked wherever a root is taken in.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let bad = |what: &'static str| serde::de::Error::custom(what);
        match RootKeyWire::deserialize(d)? {
            RootKeyWire::Phrase { public_key } => parse_hex::<32>(&public_key)
                .map(RootKey::Phrase)
                .ok_or_else(|| bad("a phrase root is 64 hex digits")),
            RootKeyWire::SecurityKey {
                public_key,
                credential_id,
            } => {
                let public_key = parse_hex::<P256_PUBLIC_KEY_LEN>(&public_key)
                    .ok_or_else(|| bad("a security key's public key is 130 hex digits"))?;
                let credential_id = hex_vec::parse(&credential_id, MAX_CREDENTIAL_ID)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| bad("a credential id is 1 to 128 bytes of hex"))?;
                Ok(RootKey::SecurityKey {
                    public_key,
                    credential_id,
                })
            }
        }
    }
}

/// Why a genesis is not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GenesisError {
    /// No roots, or more than [`MAX_ROOTS`].
    #[error("an identity has 1 to {MAX_ROOTS} roots")]
    RootCount,
    /// Not one signature for each root.
    #[error("an identity's genesis has one signature for each root")]
    SignatureCount,
    /// A root named twice.
    #[error("an identity's genesis names a root twice")]
    Duplicate,
    /// A root that is not a key any root could be.
    #[error("an identity's genesis names a root that is not a valid key")]
    InvalidRoot,
    /// A root did not sign it, as it is.
    #[error("a root did not sign its identity's genesis")]
    BadSignature,
}

/// Bytes in an identity's handle.
pub const HANDLE_LEN: usize = 32;

/// How an identity begins (P7-08): its handle, its first roots, and each
/// root's signature over both, so every root proves it is held. For a
/// security key identity the handle is random and is also the key's
/// resident credential's `user.id`, so a key put into a device that has
/// never seen it says which identity it roots.
///
/// The genesis fixes the identity's id ([`Self::user`]): later root
/// changes, made in its states, never change it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityGenesis {
    #[serde(with = "handle_hex")]
    pub handle: [u8; HANDLE_LEN],
    pub roots: Vec<RootKey>,
    /// One for each root, in `roots`' order, over [`Self::message`].
    pub signatures: Vec<RootSignature>,
}

/// Hex for serde, over a genesis handle (P7-08).
pub(crate) mod handle_hex {
    use super::*;

    pub fn serialize<S: Serializer>(handle: &[u8; HANDLE_LEN], s: S) -> Result<S::Ok, S::Error> {
        hex_vec::serialize(handle, s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; HANDLE_LEN], D::Error> {
        let text = String::deserialize(d)?;
        parse_hex(&text).ok_or_else(|| serde::de::Error::custom("a handle is 64 hex digits"))
    }
}

impl IdentityGenesis {
    /// What each root signs: [`GENESIS_DOMAIN`], the handle (32 bytes),
    /// the count of roots (u8), then each root ([`RootKey::bytes`]) in
    /// order. `None` for more roots than a byte counts.
    pub fn message(handle: &[u8; HANDLE_LEN], roots: &[RootKey]) -> Option<Vec<u8>> {
        let count = u8::try_from(roots.len()).ok()?;
        let mut message = Vec::with_capacity(GENESIS_DOMAIN.len() + 33 + 100 * roots.len());
        message.extend_from_slice(GENESIS_DOMAIN);
        message.extend_from_slice(handle);
        message.push(count);
        for root in roots {
            message.extend_from_slice(&root.bytes());
        }
        Some(message)
    }

    /// Whether it is a genesis: 1 to [`MAX_ROOTS`] valid roots, none
    /// twice, each with its signature over [`Self::message`].
    pub fn verify(&self) -> Result<(), GenesisError> {
        if self.roots.is_empty() || self.roots.len() > MAX_ROOTS {
            return Err(GenesisError::RootCount);
        }
        if self.signatures.len() != self.roots.len() {
            return Err(GenesisError::SignatureCount);
        }
        if !self.roots.iter().all(RootKey::is_valid) {
            return Err(GenesisError::InvalidRoot);
        }
        let mut seen = self.roots.clone();
        seen.sort();
        seen.dedup();
        if seen.len() != self.roots.len() {
            return Err(GenesisError::Duplicate);
        }
        let message = Self::message(&self.handle, &self.roots).ok_or(GenesisError::RootCount)?;
        if self
            .roots
            .iter()
            .zip(&self.signatures)
            .all(|(root, signature)| root.verify(&message, signature))
        {
            Ok(())
        } else {
            Err(GenesisError::BadSignature)
        }
    }

    /// The identity's id. A genesis of one phrase root keeps P7-01's id,
    /// that root's public key (the user's decision, 2026-10-11: no phrase
    /// identity moves); any other's is
    /// `blake3::derive_key(`[`USER_ID_CONTEXT`]`, message)`. Signatures
    /// are left out, since an ECDSA one differs each time it is made.
    pub fn user(&self) -> PortableUserId {
        if let [RootKey::Phrase(key)] = self.roots.as_slice() {
            return PortableUserId(*key);
        }
        let message =
            Self::message(&self.handle, &self.roots).expect("at most 255 roots in a genesis");
        PortableUserId(blake3::derive_key(USER_ID_CONTEXT, &message))
    }
}

/// What an added root signs to prove it is held, for `user`:
/// [`ROOT_ADD_DOMAIN`], the user (32 bytes), then the root
/// ([`RootKey::bytes`]).
pub fn root_add_message(user: &PortableUserId, root: &RootKey) -> Vec<u8> {
    let mut message = Vec::with_capacity(ROOT_ADD_DOMAIN.len() + 32 + 100);
    message.extend_from_slice(ROOT_ADD_DOMAIN);
    message.extend_from_slice(user.as_bytes());
    message.extend_from_slice(&root.bytes());
    message
}

/// What the root that adds another signs, for `user`:
/// [`ROOT_ENDORSE_DOMAIN`], the user (32 bytes), the added root
/// ([`RootKey::bytes`]), then when it was added (i64, big-endian).
pub fn root_endorse_message(user: &PortableUserId, root: &RootKey, added_at: i64) -> Vec<u8> {
    let mut message = Vec::with_capacity(ROOT_ENDORSE_DOMAIN.len() + 32 + 100 + 8);
    message.extend_from_slice(ROOT_ENDORSE_DOMAIN);
    message.extend_from_slice(user.as_bytes());
    message.extend_from_slice(&root.bytes());
    message.extend_from_slice(&added_at.to_be_bytes());
    message
}

/// A root added to an identity, self-certifying: who added it and when,
/// that root's endorsement over [`root_endorse_message`], and the new
/// root's proof that it is held, over [`root_add_message`]. A state lists
/// every one so far, so any state carries the whole chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddedRoot {
    pub root: RootKey,
    pub added_by: RootKey,
    /// Unix seconds.
    pub added_at: i64,
    pub endorsement: RootSignature,
    pub possession: RootSignature,
}

impl AddedRoot {
    /// Whether its root is valid and both signatures hold: the root's
    /// over its addition, the adder's over its endorsement.
    pub fn verify(&self, user: &PortableUserId) -> bool {
        self.root.is_valid()
            && self
                .root
                .verify(&root_add_message(user, &self.root), &self.possession)
            && self.added_by.verify(
                &root_endorse_message(user, &self.root, self.added_at),
                &self.endorsement,
            )
    }
}

/// A root a state removes, and when (Unix seconds). A removed root never
/// comes back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovedRoot {
    pub root: RootKey,
    pub removed_at: i64,
}

/// The roots that may act for an identity, as a verifier works them out
/// from its genesis and every valid state it holds (P7-08):
///
/// - `ever`: the genesis's roots, and every root added by one in `ever`,
///   until nothing more is added. A state counts only if one of these
///   signed it.
/// - `removed`: every root a state signed by one in `ever` removes, at
///   its earliest `removed_at`. Removal never lapses: a removed root never
///   comes back.
/// - `members`: the genesis's roots, and every root added by a member
///   no later than that member's own removal (`added_at` at or before
///   its `removed_at`: times are whole seconds, and a rotation adds the
///   new key and drops the old one in the same one), until nothing more
///   is added; then without the removed. A credential counts only from a
///   member.
///
/// So a root added by a key later rotated out stays, and one a removed
/// key adds after its removal does not. Times are the roots' own word:
/// a stolen key that backdates an addition is stopped where its removal
/// arrived first, by the verifier refusing the state that brings it
/// (`spoolframe_server`'s store, the SDK's `accept_state`). A removed
/// root's removals still count, so two roots that remove each other are
/// both gone: the identity is locked rather than taken (P7-07 reviews
/// this).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootSet {
    pub ever: Vec<RootKey>,
    pub removed: Vec<(RootKey, i64)>,
    pub members: Vec<RootKey>,
}

/// A state as the root set reads it: who signed it, what it adds and
/// removes. Its signatures are assumed checked.
pub struct RootChanges<'a> {
    pub signer: &'a RootKey,
    pub added: &'a [AddedRoot],
    pub removed: &'a [RemovedRoot],
}

impl RootSet {
    /// The set `genesis` begins and `states` change.
    pub fn of(genesis: &[RootKey], states: &[RootChanges<'_>]) -> Self {
        let additions: Vec<&AddedRoot> = states.iter().flat_map(|s| s.added).collect();
        let closure = |admit: &dyn Fn(&[RootKey], &AddedRoot) -> bool| {
            let mut set = genesis.to_vec();
            loop {
                let before = set.len();
                for added in &additions {
                    if !set.contains(&added.root) && admit(&set, added) {
                        set.push(added.root.clone());
                    }
                }
                if set.len() == before {
                    return set;
                }
            }
        };
        let ever = closure(&|set, added| set.contains(&added.added_by));
        let mut removed: Vec<(RootKey, i64)> = Vec::new();
        for state in states.iter().filter(|s| ever.contains(s.signer)) {
            for root in state.removed {
                match removed.iter_mut().find(|(r, _)| *r == root.root) {
                    Some((_, at)) => *at = (*at).min(root.removed_at),
                    None => removed.push((root.root.clone(), root.removed_at)),
                }
            }
        }
        let removed_at =
            |root: &RootKey| removed.iter().find(|(r, _)| r == root).map(|(_, at)| *at);
        let members = closure(&|set, added| {
            set.contains(&added.added_by)
                && removed_at(&added.added_by).is_none_or(|at| added.added_at <= at)
        })
        .into_iter()
        .filter(|root| removed_at(root).is_none())
        .collect();
        RootSet {
            ever,
            removed,
            members,
        }
    }

    /// Whether `root` may act now: a member, not removed.
    pub fn holds(&self, root: &RootKey) -> bool {
        self.members.contains(root)
    }

    /// Whether `root` was ever one of the identity's: a state it signed
    /// is taken in, for its removals.
    pub fn knew(&self, root: &RootKey) -> bool {
        self.ever.contains(root)
    }

    /// Whether `root` is removed.
    pub fn is_removed(&self, root: &RootKey) -> bool {
        self.removed.iter().any(|(r, _)| r == root)
    }

    /// Whether `added`, arriving now, adds a root this set does not know
    /// whose adder this set has already removed: an addition that came
    /// after the removal, whatever time it claims. A verifier refuses the
    /// state that brings it, so a stolen key cannot backdate its way past
    /// a removal the verifier already holds.
    pub fn refuses_additions(&self, added: &[AddedRoot]) -> bool {
        added
            .iter()
            .any(|added| !self.knew(&added.root) && self.is_removed(&added.added_by))
    }
}
