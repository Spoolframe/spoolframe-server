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
//!
//! A platform's WebAuthn API (Apple's system security-key API) does not
//! let the app choose `clientDataHash`: the platform writes a
//! `clientDataJSON` around the app's `challenge` and the key signs
//! `authenticatorData ‖ SHA-256(clientDataJSON)`. So a root signature
//! comes in a second form, [`RootSignature::WebAuthn`], whose challenge
//! is the same SHA-256 of the same bytes. Both forms verify against the
//! same [`RootKey::SecurityKey`]: a credential is bound only to its RP id
//! hash, so a key registered on Linux signs on an iPhone, and the reverse.

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

/// The longest `clientDataJSON` a WebAuthn root signature or registration
/// may carry, refused before it is parsed. A platform's is a few hundred
/// bytes.
pub const MAX_CLIENT_DATA_JSON: usize = 4096;

/// The longest WebAuthn `attestationObject` a registration may carry,
/// refused before it is parsed. Room for an attestation certificate chain,
/// which is carried but not checked.
pub const MAX_ATTESTATION_OBJECT: usize = 8192;

/// `clientDataJSON` `type` of a WebAuthn assertion.
const WEBAUTHN_GET: &str = "webauthn.get";
/// `clientDataJSON` `type` of a WebAuthn registration.
const WEBAUTHN_CREATE: &str = "webauthn.create";
/// `authenticatorData` flag: attested credential data follows the counter.
const FLAG_AT: u8 = 0x40;
/// `authenticatorData` flag: extensions follow.
const FLAG_ED: u8 = 0x80;

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
    /// verified, over `authenticatorData ‖ SHA-256(message)`; or the same
    /// key's WebAuthn assertion whose challenge is SHA-256(`message`)
    /// ([`RootSignature::WebAuthn`]). The
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
            (
                RootKey::SecurityKey { public_key, .. },
                RootSignature::WebAuthn {
                    authenticator_data,
                    client_data_json,
                    signature,
                },
            ) => verify_webauthn(
                public_key,
                message,
                authenticator_data,
                client_data_json,
                signature,
            )
            .is_ok(),
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

/// Why a WebAuthn assertion or registration is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WebAuthnError {
    /// A field over its bound ([`MAX_CLIENT_DATA_JSON`],
    /// [`MAX_ATTESTATION_OBJECT`], [`MAX_AUTHENTICATOR_DATA`],
    /// [`MAX_DER_SIGNATURE`]), refused before it is parsed.
    #[error("a WebAuthn field is too long")]
    TooLong,
    /// `clientDataJSON` is not a JSON object with string `type` and
    /// `challenge` (and, if present, a boolean `crossOrigin`).
    #[error("the clientDataJSON is not WebAuthn client data")]
    MalformedClientData,
    /// `webauthn.create` where an assertion was wanted, or the reverse.
    #[error("the clientDataJSON is of the wrong type")]
    WrongType,
    /// The challenge is not the one this signature or registration is for.
    #[error("the clientDataJSON's challenge is not this message's")]
    WrongChallenge,
    /// Made in a cross-origin frame.
    #[error("the clientDataJSON is cross-origin")]
    CrossOrigin,
    /// `authenticatorData` too short to hold its RP id hash, flags and
    /// counter, or its attested credential data does not parse.
    #[error("the authenticatorData does not parse")]
    MalformedAuthenticatorData,
    /// Not under [`PORTABLE_RP`].
    #[error("the authenticatorData is another relying party's")]
    WrongRp,
    /// Made without the touch (UP) or the PIN (UV).
    #[error("the key was not touched, or no PIN was given")]
    NotVerified,
    /// The signature is not the root's.
    #[error("the signature does not check out")]
    BadSignature,
    /// The `attestationObject` is not CBOR with text `fmt` and bytes
    /// `authData`.
    #[error("the attestationObject does not parse")]
    MalformedAttestation,
    /// A registration whose `authenticatorData` carries no credential (AT).
    #[error("the registration carries no credential")]
    NoCredential,
    /// A credential id empty or over [`MAX_CREDENTIAL_ID`] bytes.
    #[error("the credential id is empty or too long")]
    BadCredentialId,
    /// A credential public key that is not an ES256 P-256 COSE key.
    #[error("the credential is not an ES256 P-256 key")]
    UnsupportedKey,
}

/// Check `client_data_json` is WebAuthn client data of type `kind` whose
/// challenge is `challenge` (base64url, no padding: compared as the
/// canonical encoding, so no other spelling of the same bytes passes).
///
/// `origin` is not checked. A native app's platform writes its own origin
/// (an app may differ from `https://portable.spoolframe.app`), and what
/// binds a signature to Spoolframe is the RP id hash in
/// `authenticatorData`, which the platform sets from the RP id the app is
/// entitled to, and which the verifier checks.
fn check_client_data(
    client_data_json: &[u8],
    kind: &str,
    challenge: &[u8],
) -> Result<(), WebAuthnError> {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    if client_data_json.len() > MAX_CLIENT_DATA_JSON {
        return Err(WebAuthnError::TooLong);
    }
    let data =
        super::client_data::parse(client_data_json).ok_or(WebAuthnError::MalformedClientData)?;
    let (Some(kind_read), Some(challenge_read)) = (data.kind, data.challenge) else {
        return Err(WebAuthnError::MalformedClientData);
    };
    let cross_origin = data
        .cross_origin
        .transpose()
        .map_err(|()| WebAuthnError::MalformedClientData)?;
    if kind_read != kind {
        return Err(WebAuthnError::WrongType);
    }
    if challenge_read != URL_SAFE_NO_PAD.encode(challenge) {
        return Err(WebAuthnError::WrongChallenge);
    }
    if cross_origin == Some(true) {
        return Err(WebAuthnError::CrossOrigin);
    }
    Ok(())
}

/// Check `authenticator_data` is under [`PORTABLE_RP`] with UP and UV;
/// its flags.
fn check_authenticator_data(authenticator_data: &[u8]) -> Result<u8, WebAuthnError> {
    if authenticator_data.len() < MIN_AUTHENTICATOR_DATA {
        return Err(WebAuthnError::MalformedAuthenticatorData);
    }
    if authenticator_data[..RP_ID_HASH_LEN] != portable_rp_id_hash() {
        return Err(WebAuthnError::WrongRp);
    }
    let flags = authenticator_data[RP_ID_HASH_LEN];
    if flags & FLAG_UP == 0 || flags & FLAG_UV == 0 {
        return Err(WebAuthnError::NotVerified);
    }
    Ok(flags)
}

/// Whether the WebAuthn assertion is `public_key`'s over `message`, in
/// order: bounds; `clientDataJSON` of type `webauthn.get` with challenge
/// SHA-256(`message`), not cross-origin; `authenticatorData` under
/// [`PORTABLE_RP`] with UP and UV; the ES256 signature over
/// `authenticatorData ‖ SHA-256(clientDataJSON)`. The counter is not
/// looked at, as for the CTAP2 form.
pub(crate) fn verify_webauthn(
    public_key: &[u8; P256_PUBLIC_KEY_LEN],
    message: &[u8],
    authenticator_data: &[u8],
    client_data_json: &[u8],
    der: &[u8],
) -> Result<(), WebAuthnError> {
    if client_data_json.len() > MAX_CLIENT_DATA_JSON
        || authenticator_data.len() > MAX_AUTHENTICATOR_DATA
        || der.len() > MAX_DER_SIGNATURE
    {
        return Err(WebAuthnError::TooLong);
    }
    check_client_data(client_data_json, WEBAUTHN_GET, &Sha256::digest(message))?;
    check_authenticator_data(authenticator_data)?;
    let (Ok(key), Ok(signature)) = (
        p256::ecdsa::VerifyingKey::from_sec1_bytes(public_key),
        p256::ecdsa::Signature::from_der(der),
    ) else {
        return Err(WebAuthnError::BadSignature);
    };
    let mut signed = Vec::with_capacity(authenticator_data.len() + 32);
    signed.extend_from_slice(authenticator_data);
    signed.extend_from_slice(&Sha256::digest(client_data_json));
    key.verify(&signed, &signature)
        .map_err(|_| WebAuthnError::BadSignature)
}

/// How a security key made a root's credential (P7-08), as a device's
/// key API returns it: what [`Self::root`] reads the root from.
///
/// A registration is not a proof that the key is held, and is not sent
/// anywhere: a root proves itself by signing its identity's genesis, or
/// its addition ([`root_add_message`]), with an assertion. Its attestation
/// statement is carried but never checked, whatever its `fmt` (`none`,
/// `packed`, `apple`, ...), as the raw CTAP2 form's never was: no
/// authenticator model is required of a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootRegistration {
    /// Raw CTAP2 `authenticatorMakeCredential` (libfido2): the credential
    /// as the transport read it. Its client data hash is the identity's
    /// handle.
    Ctap(RootKey),
    /// A WebAuthn registration, as a platform API returns it (Apple's
    /// `ASAuthorizationSecurityKeyPublicKeyCredentialRegistration`): the
    /// `attestationObject` (CBOR) and the exact `clientDataJSON`, whose
    /// challenge is the identity's handle, as the CTAP2 form's client
    /// data hash is.
    WebAuthn {
        attestation_object: Vec<u8>,
        client_data_json: Vec<u8>,
    },
}

impl RootRegistration {
    /// The root this registration made for the identity `handle`.
    ///
    /// The raw CTAP2 form's root as given: the caller checks it
    /// ([`RootKey::is_valid`]), as it always has. The WebAuthn form, in
    /// order: bounds ([`MAX_ATTESTATION_OBJECT`], [`MAX_CLIENT_DATA_JSON`]);
    /// `clientDataJSON` of type `webauthn.create` with challenge `handle`
    /// (base64url, no padding), not cross-origin; the `attestationObject`
    /// a CBOR map with text `fmt`, bytes `authData` and map `attStmt`;
    /// `authData` under [`PORTABLE_RP`], with UP, UV and attested
    /// credential data (AT); a credential id of 1 to
    /// [`MAX_CREDENTIAL_ID`] bytes; an ES256 P-256 COSE key on the curve.
    ///
    /// That the credential is resident, with `user.id` = `handle`, is
    /// asked of the key but cannot be read from a registration (a key does
    /// not report it there), so it is not checked: a key that ignored it
    /// is found out when recovery by handle finds nothing.
    pub fn root(&self, handle: &[u8; HANDLE_LEN]) -> Result<RootKey, WebAuthnError> {
        match self {
            RootRegistration::Ctap(root) => Ok(root.clone()),
            RootRegistration::WebAuthn {
                attestation_object,
                client_data_json,
            } => webauthn_registration_root(attestation_object, client_data_json, handle),
        }
    }
}

fn webauthn_registration_root(
    attestation_object: &[u8],
    client_data_json: &[u8],
    handle: &[u8; HANDLE_LEN],
) -> Result<RootKey, WebAuthnError> {
    if attestation_object.len() > MAX_ATTESTATION_OBJECT
        || client_data_json.len() > MAX_CLIENT_DATA_JSON
    {
        return Err(WebAuthnError::TooLong);
    }
    check_client_data(client_data_json, WEBAUTHN_CREATE, handle)?;
    let auth_data = attestation_auth_data(attestation_object)?;
    let flags = check_authenticator_data(&auth_data)?;
    if flags & FLAG_AT == 0 {
        return Err(WebAuthnError::NoCredential);
    }
    let malformed = WebAuthnError::MalformedAuthenticatorData;
    // AAGUID (16), credential id length (u16), credential id, COSE key.
    let attested = &auth_data[MIN_AUTHENTICATOR_DATA..];
    let id_len = attested.get(16..18).ok_or(malformed)?;
    let id_len = usize::from(u16::from_be_bytes([id_len[0], id_len[1]]));
    if id_len == 0 || id_len > MAX_CREDENTIAL_ID {
        return Err(WebAuthnError::BadCredentialId);
    }
    let credential_id = attested.get(18..18 + id_len).ok_or(malformed)?.to_vec();
    let mut rest = &attested[18 + id_len..];
    let cose: ciborium::Value =
        ciborium::de::from_reader_with_recursion_limit(&mut rest, 4).map_err(|_| malformed)?;
    // What follows the key is the extensions', if flagged; else nothing.
    if flags & FLAG_ED == 0 && !rest.is_empty() {
        return Err(malformed);
    }
    let public_key = cose_es256_key(cose)?;
    let root = RootKey::SecurityKey {
        public_key,
        credential_id,
    };
    if !root.is_valid() {
        return Err(WebAuthnError::UnsupportedKey);
    }
    Ok(root)
}

/// The `authData` of a WebAuthn `attestationObject`: a CBOR map, nothing
/// after it, each key once, with text `fmt`, map `attStmt` (not looked
/// into) and bytes `authData`.
fn attestation_auth_data(attestation_object: &[u8]) -> Result<Vec<u8>, WebAuthnError> {
    use ciborium::Value;

    let malformed = WebAuthnError::MalformedAttestation;
    let mut rest = attestation_object;
    let Value::Map(members) =
        ciborium::de::from_reader_with_recursion_limit(&mut rest, 16).map_err(|_| malformed)?
    else {
        return Err(malformed);
    };
    if !rest.is_empty() {
        return Err(malformed);
    }
    let (mut fmt, mut statement, mut auth_data) = (false, false, None);
    for (key, value) in members {
        let seen = match (key.as_text(), value) {
            (Some("fmt"), Value::Text(_)) => std::mem::replace(&mut fmt, true),
            (Some("attStmt"), Value::Map(_)) => std::mem::replace(&mut statement, true),
            (Some("authData"), Value::Bytes(bytes)) => auth_data.replace(bytes).is_some(),
            (Some("fmt" | "attStmt" | "authData"), _) => return Err(malformed),
            _ => false,
        };
        if seen {
            return Err(malformed);
        }
    }
    match auth_data {
        Some(auth_data) if fmt && statement => Ok(auth_data),
        _ => Err(malformed),
    }
}

/// The SEC1 point of an ES256 P-256 COSE key (RFC 9053): kty 2 (EC2),
/// alg -7 (ES256), crv 1 (P-256), and 32-byte x and y. A key given twice,
/// or one that is not an integer, is malformed; any other key or value,
/// unsupported.
fn cose_es256_key(cose: ciborium::Value) -> Result<[u8; P256_PUBLIC_KEY_LEN], WebAuthnError> {
    use ciborium::Value;

    let malformed = WebAuthnError::MalformedAuthenticatorData;
    let Value::Map(members) = cose else {
        return Err(malformed);
    };
    let mut seen: Vec<i128> = Vec::with_capacity(members.len());
    let (mut kty, mut alg, mut crv, mut x, mut y) = (None, None, None, None, None);
    for (key, value) in members {
        let key = i128::from(key.as_integer().ok_or(malformed)?);
        if seen.contains(&key) {
            return Err(malformed);
        }
        seen.push(key);
        let integer = value.as_integer().map(i128::from);
        match key {
            1 => kty = integer,
            3 => alg = integer,
            -1 => crv = integer,
            -2 => x = value.into_bytes().ok(),
            -3 => y = value.into_bytes().ok(),
            _ => {}
        }
    }
    let (Some(2), Some(-7), Some(1), Some(x), Some(y)) = (kty, alg, crv, x, y) else {
        return Err(WebAuthnError::UnsupportedKey);
    };
    if x.len() != 32 || y.len() != 32 {
        return Err(WebAuthnError::UnsupportedKey);
    }
    let mut point = [0u8; P256_PUBLIC_KEY_LEN];
    point[0] = 4;
    point[1..33].copy_from_slice(&x);
    point[33..].copy_from_slice(&y);
    Ok(point)
}

/// A root's signature: a phrase root's Ed25519 one, or a security key's
/// assertion: raw CTAP2 (its `authenticatorData` and DER ECDSA
/// signature), or WebAuthn (the same, and the `clientDataJSON` the
/// platform wrote and the key signed the hash of).
///
/// In serde, an Ed25519 signature is 128 hex digits, as P7-02 wrote it,
/// so a phrase root's credential and state read as they always did; a
/// CTAP2 assertion is `{"authenticator_data": hex, "signature": hex}`; a
/// WebAuthn one is `{"authenticator_data": hex, "client_data_json": hex,
/// "signature": hex}` (wire revision 14, additive: no struct grows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootSignature {
    Ed25519(Signature),
    Ctap {
        authenticator_data: Vec<u8>,
        signature: Vec<u8>,
    },
    /// A WebAuthn assertion, as a platform API returns it (Apple's
    /// `ASAuthorizationSecurityKeyPublicKeyCredentialAssertion`): the
    /// challenge in `client_data_json` is SHA-256 of the message, the
    /// bytes the CTAP2 form's `clientDataHash` is. The credential id is
    /// the root's.
    WebAuthn {
        authenticator_data: Vec<u8>,
        /// The exact bytes the platform returned (`rawClientDataJSON`),
        /// at most [`MAX_CLIENT_DATA_JSON`].
        client_data_json: Vec<u8>,
        signature: Vec<u8>,
    },
}

impl RootSignature {
    /// The Ed25519 signature, for a phrase root's.
    pub fn as_ed25519(&self) -> Option<&Signature> {
        match self {
            RootSignature::Ed25519(signature) => Some(signature),
            RootSignature::Ctap { .. } | RootSignature::WebAuthn { .. } => None,
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
#[serde(deny_unknown_fields)]
struct CtapWire {
    #[serde(serialize_with = "hex_vec::serialize")]
    #[serde(deserialize_with = "authenticator_data_hex")]
    authenticator_data: Vec<u8>,
    #[serde(serialize_with = "hex_vec::serialize")]
    #[serde(deserialize_with = "der_hex")]
    signature: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebAuthnWire {
    #[serde(serialize_with = "hex_vec::serialize")]
    #[serde(deserialize_with = "authenticator_data_hex")]
    authenticator_data: Vec<u8>,
    #[serde(serialize_with = "hex_vec::serialize")]
    #[serde(deserialize_with = "client_data_json_hex")]
    client_data_json: Vec<u8>,
    #[serde(serialize_with = "hex_vec::serialize")]
    #[serde(deserialize_with = "der_hex")]
    signature: Vec<u8>,
}

fn client_data_json_hex<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let text = String::deserialize(d)?;
    hex_vec::parse(&text, MAX_CLIENT_DATA_JSON)
        .ok_or_else(|| serde::de::Error::custom("client data JSON is at most 4096 bytes of hex"))
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
    // Before `Ctap`, and both refuse unknown fields, so neither form reads
    // as the other.
    WebAuthn(WebAuthnWire),
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
            RootSignature::WebAuthn {
                authenticator_data,
                client_data_json,
                signature,
            } => WebAuthnWire {
                authenticator_data: authenticator_data.clone(),
                client_data_json: client_data_json.clone(),
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
            RootSignatureWire::WebAuthn(WebAuthnWire {
                authenticator_data,
                client_data_json,
                signature,
            }) => Ok(RootSignature::WebAuthn {
                authenticator_data,
                client_data_json,
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

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use p256::ecdsa::signature::Signer;
    use p256::ecdsa::{DerSignature, SigningKey};

    use super::*;

    const MESSAGE: &[u8] = b"spoolframe root add v1\0some bytes";

    fn signing(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32].into()).unwrap()
    }

    fn public(key: &SigningKey) -> [u8; P256_PUBLIC_KEY_LEN] {
        key.verifying_key()
            .to_sec1_point(false)
            .as_bytes()
            .try_into()
            .unwrap()
    }

    fn client_data(kind: &str, challenge: &[u8]) -> Vec<u8> {
        format!(
            r#"{{"type":"{kind}","challenge":"{}","origin":"https://portable.spoolframe.app"}}"#,
            URL_SAFE_NO_PAD.encode(challenge)
        )
        .into_bytes()
    }

    fn authenticator_data(rp: &str, flags: u8) -> Vec<u8> {
        let mut data = Sha256::digest(rp.as_bytes()).to_vec();
        data.push(flags);
        data.extend_from_slice(&7u32.to_be_bytes());
        data
    }

    /// What a platform returns: `key`'s signature over
    /// `authenticator_data ‖ SHA-256(client_data_json)`, made properly, so
    /// only the part a test changes is wrong.
    fn signed(
        key: &SigningKey,
        authenticator_data: Vec<u8>,
        client_data_json: Vec<u8>,
    ) -> RootSignature {
        let mut bytes = authenticator_data.clone();
        bytes.extend_from_slice(&Sha256::digest(&client_data_json));
        let signature: DerSignature = key.sign(&bytes);
        RootSignature::WebAuthn {
            authenticator_data,
            client_data_json,
            signature: signature.as_bytes().to_vec(),
        }
    }

    fn good(key: &SigningKey, message: &[u8]) -> RootSignature {
        signed(
            key,
            authenticator_data(PORTABLE_RP, 0x05),
            client_data(WEBAUTHN_GET, &Sha256::digest(message)),
        )
    }

    fn check(
        key: &SigningKey,
        message: &[u8],
        signature: &RootSignature,
    ) -> Result<(), WebAuthnError> {
        let RootSignature::WebAuthn {
            authenticator_data,
            client_data_json,
            signature,
        } = signature
        else {
            unreachable!()
        };
        verify_webauthn(
            &public(key),
            message,
            authenticator_data,
            client_data_json,
            signature,
        )
    }

    fn root(key: &SigningKey) -> RootKey {
        RootKey::SecurityKey {
            public_key: public(key),
            credential_id: vec![1; 32],
        }
    }

    #[test]
    fn a_webauthn_assertion_over_the_message_s_hash_verifies() {
        let key = signing(3);
        assert_eq!(check(&key, MESSAGE, &good(&key, MESSAGE)), Ok(()));
        assert!(root(&key).verify(MESSAGE, &good(&key, MESSAGE)));
    }

    #[test]
    fn a_webauthn_assertion_of_the_wrong_type_is_refused() {
        let key = signing(3);
        let made = signed(
            &key,
            authenticator_data(PORTABLE_RP, 0x05),
            client_data("payment.get", &Sha256::digest(MESSAGE)),
        );
        assert_eq!(check(&key, MESSAGE, &made), Err(WebAuthnError::WrongType));
    }

    /// A registration's client data, signed by the credential's own key
    /// as a packed self-attestation is, over the right challenge: the type
    /// is what keeps it from passing as an assertion.
    #[test]
    fn a_webauthn_create_presented_as_an_assertion_is_refused() {
        let key = signing(3);
        let made = signed(
            &key,
            authenticator_data(PORTABLE_RP, 0x45),
            client_data(WEBAUTHN_CREATE, &Sha256::digest(MESSAGE)),
        );
        assert_eq!(check(&key, MESSAGE, &made), Err(WebAuthnError::WrongType));
    }

    #[test]
    fn a_webauthn_assertion_with_a_wrong_challenge_is_refused() {
        let key = signing(3);
        // The message itself, not its hash.
        let made = signed(
            &key,
            authenticator_data(PORTABLE_RP, 0x05),
            client_data(WEBAUTHN_GET, MESSAGE),
        );
        assert_eq!(
            check(&key, MESSAGE, &made),
            Err(WebAuthnError::WrongChallenge)
        );
        // The hash, padded: another spelling of the same bytes.
        let padded = format!(
            r#"{{"type":"webauthn.get","challenge":"{}="}}"#,
            URL_SAFE_NO_PAD.encode(Sha256::digest(MESSAGE))
        );
        let made = signed(
            &key,
            authenticator_data(PORTABLE_RP, 0x05),
            padded.into_bytes(),
        );
        assert_eq!(
            check(&key, MESSAGE, &made),
            Err(WebAuthnError::WrongChallenge)
        );
    }

    #[test]
    fn a_webauthn_assertion_over_another_message_is_refused() {
        let key = signing(3);
        let other = good(&key, b"spoolframe root add v1\0other bytes");
        assert_eq!(
            check(&key, MESSAGE, &other),
            Err(WebAuthnError::WrongChallenge)
        );
        assert!(!root(&key).verify(MESSAGE, &other));
    }

    #[test]
    fn a_cross_origin_webauthn_assertion_is_refused_and_a_false_one_is_not() {
        let key = signing(3);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(MESSAGE));
        let with = |cross: &str| {
            signed(
                &key,
                authenticator_data(PORTABLE_RP, 0x05),
                format!(
                    r#"{{"type":"webauthn.get","challenge":"{challenge}","crossOrigin":{cross}}}"#
                )
                .into_bytes(),
            )
        };
        assert_eq!(
            check(&key, MESSAGE, &with("true")),
            Err(WebAuthnError::CrossOrigin)
        );
        assert_eq!(check(&key, MESSAGE, &with("false")), Ok(()));
    }

    /// The origin is not what binds it; any origin with the right RP id
    /// hash verifies.
    #[test]
    fn a_webauthn_assertion_with_another_origin_verifies() {
        let key = signing(3);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(MESSAGE));
        let made = signed(
            &key,
            authenticator_data(PORTABLE_RP, 0x05),
            format!(r#"{{"type":"webauthn.get","challenge":"{challenge}","origin":"ios:bundle-id:app.spoolframe"}}"#)
                .into_bytes(),
        );
        assert_eq!(check(&key, MESSAGE, &made), Ok(()));
    }

    #[test]
    fn a_webauthn_assertion_under_another_rp_is_refused() {
        let key = signing(3);
        let made = signed(
            &key,
            authenticator_data("spoolframe.app", 0x05),
            client_data(WEBAUTHN_GET, &Sha256::digest(MESSAGE)),
        );
        assert_eq!(check(&key, MESSAGE, &made), Err(WebAuthnError::WrongRp));
    }

    #[test]
    fn a_webauthn_assertion_without_up_or_uv_is_refused() {
        let key = signing(3);
        for flags in [0x01, 0x04, 0x00] {
            let made = signed(
                &key,
                authenticator_data(PORTABLE_RP, flags),
                client_data(WEBAUTHN_GET, &Sha256::digest(MESSAGE)),
            );
            assert_eq!(
                check(&key, MESSAGE, &made),
                Err(WebAuthnError::NotVerified),
                "{flags:#x}"
            );
        }
    }

    #[test]
    fn a_webauthn_assertion_with_a_bad_signature_is_refused() {
        let key = signing(3);
        let RootSignature::WebAuthn {
            authenticator_data,
            client_data_json,
            mut signature,
        } = good(&key, MESSAGE)
        else {
            unreachable!()
        };
        let last = signature.len() - 1;
        signature[last] ^= 1;
        let made = RootSignature::WebAuthn {
            authenticator_data,
            client_data_json,
            signature,
        };
        assert_eq!(
            check(&key, MESSAGE, &made),
            Err(WebAuthnError::BadSignature)
        );
    }

    #[test]
    fn a_webauthn_assertion_from_another_credential_is_refused() {
        let (key, other) = (signing(3), signing(4));
        assert_eq!(
            check(&key, MESSAGE, &good(&other, MESSAGE)),
            Err(WebAuthnError::BadSignature)
        );
        assert!(!root(&key).verify(MESSAGE, &good(&other, MESSAGE)));
    }

    #[test]
    fn an_oversize_client_data_json_is_refused_before_it_parses() {
        let key = signing(3);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(MESSAGE));
        let mut json =
            format!(r#"{{"type":"webauthn.get","challenge":"{challenge}","pad":""#).into_bytes();
        json.resize(MAX_CLIENT_DATA_JSON - 2, b'x');
        json.extend_from_slice(b"\"}");
        assert_eq!(json.len(), MAX_CLIENT_DATA_JSON);
        let at_bound = signed(&key, authenticator_data(PORTABLE_RP, 0x05), json.clone());
        assert_eq!(check(&key, MESSAGE, &at_bound), Ok(()));
        json.insert(json.len() - 2, b'x');
        let over = signed(&key, authenticator_data(PORTABLE_RP, 0x05), json);
        assert_eq!(check(&key, MESSAGE, &over), Err(WebAuthnError::TooLong));
    }

    #[test]
    fn malformed_client_data_json_is_refused() {
        let key = signing(3);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(MESSAGE));
        for json in [
            b"".to_vec(),
            b"not json".to_vec(),
            b"[]".to_vec(),
            b"\xff\xfe".to_vec(),
            format!(r#"{{"challenge":"{challenge}"}}"#).into_bytes(),
            br#"{"type":"webauthn.get"}"#.to_vec(),
            format!(r#"{{"type":1,"challenge":"{challenge}"}}"#).into_bytes(),
            format!(r#"{{"type":"webauthn.get","challenge":"{challenge}","crossOrigin":"no"}}"#)
                .into_bytes(),
            // A member twice: which one a reader takes is not to be guessed.
            format!(
                r#"{{"type":"webauthn.create","type":"webauthn.get","challenge":"{challenge}"}}"#
            )
            .into_bytes(),
            format!(r#"{{"type":"webauthn.get","challenge":"{challenge}""#).into_bytes(),
        ] {
            let made = signed(&key, authenticator_data(PORTABLE_RP, 0x05), json.clone());
            assert_eq!(
                check(&key, MESSAGE, &made),
                Err(WebAuthnError::MalformedClientData),
                "{}",
                String::from_utf8_lossy(&json)
            );
        }
    }

    #[test]
    fn oversize_or_short_webauthn_fields_are_refused() {
        let key = signing(3);
        let RootSignature::WebAuthn {
            authenticator_data,
            client_data_json,
            signature,
        } = good(&key, MESSAGE)
        else {
            unreachable!()
        };
        let mut long_data = authenticator_data.clone();
        long_data.resize(MAX_AUTHENTICATOR_DATA + 1, 0);
        let mut long_signature = signature.clone();
        long_signature.resize(MAX_DER_SIGNATURE + 1, 0);
        let public = public(&key);
        assert_eq!(
            verify_webauthn(&public, MESSAGE, &long_data, &client_data_json, &signature),
            Err(WebAuthnError::TooLong)
        );
        assert_eq!(
            verify_webauthn(
                &public,
                MESSAGE,
                &authenticator_data,
                &client_data_json,
                &long_signature
            ),
            Err(WebAuthnError::TooLong)
        );
        assert_eq!(
            verify_webauthn(
                &public,
                MESSAGE,
                &authenticator_data[..36],
                &client_data_json,
                &signature
            ),
            Err(WebAuthnError::MalformedAuthenticatorData)
        );
    }

    /// A phrase root takes no WebAuthn signature, and a security key root
    /// no Ed25519 one.
    #[test]
    fn a_webauthn_signature_is_no_phrase_root_s() {
        let key = signing(3);
        let phrase = RootKey::Phrase(
            ed25519_dalek::SigningKey::from_bytes(&[5; 32])
                .verifying_key()
                .to_bytes(),
        );
        assert!(!phrase.verify(MESSAGE, &good(&key, MESSAGE)));
    }

    #[test]
    fn a_webauthn_signature_round_trips_and_reads_as_neither_other_form() {
        let key = signing(3);
        let made = good(&key, MESSAGE);
        let json = serde_json::to_value(&made).unwrap();
        assert_eq!(
            json.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["authenticator_data", "client_data_json", "signature"]
        );
        assert_eq!(
            serde_json::from_value::<RootSignature>(json.clone()).unwrap(),
            made
        );
        let mut cbor = Vec::new();
        ciborium::into_writer(&made, &mut cbor).unwrap();
        assert_eq!(
            ciborium::from_reader::<RootSignature, _>(cbor.as_slice()).unwrap(),
            made
        );
        // A CTAP2 assertion with an extra member is neither form.
        let mut extra = json.clone();
        extra.as_object_mut().unwrap().remove("client_data_json");
        extra
            .as_object_mut()
            .unwrap()
            .insert("origin".into(), "00".into());
        assert!(serde_json::from_value::<RootSignature>(extra).is_err());
        // An oversize client data JSON does not read, nor fall back to CTAP2.
        let mut over = json;
        over["client_data_json"] = "00".repeat(MAX_CLIENT_DATA_JSON + 1).into();
        assert!(serde_json::from_value::<RootSignature>(over).is_err());
    }

    // Registration.

    const HANDLE: [u8; HANDLE_LEN] = [0x42; HANDLE_LEN];

    fn cose_key(key: &SigningKey) -> ciborium::Value {
        use ciborium::Value;
        let point = key.verifying_key().to_sec1_point(false);
        Value::Map(vec![
            (Value::from(1), Value::from(2)),
            (Value::from(3), Value::from(-7)),
            (Value::from(-1), Value::from(1)),
            (Value::from(-2), Value::Bytes(point.x().unwrap().to_vec())),
            (Value::from(-3), Value::Bytes(point.y().unwrap().to_vec())),
        ])
    }

    fn cbor(value: &ciborium::Value) -> Vec<u8> {
        let mut bytes = Vec::new();
        ciborium::into_writer(value, &mut bytes).unwrap();
        bytes
    }

    fn registration_data(rp: &str, flags: u8, credential_id: &[u8], cose: &[u8]) -> Vec<u8> {
        let mut data = authenticator_data(rp, flags);
        data.extend_from_slice(&[0xaa; 16]);
        data.extend_from_slice(&u16::try_from(credential_id.len()).unwrap().to_be_bytes());
        data.extend_from_slice(credential_id);
        data.extend_from_slice(cose);
        data
    }

    fn attestation(fmt: &str, auth_data: Vec<u8>) -> Vec<u8> {
        use ciborium::Value;
        cbor(&Value::Map(vec![
            (Value::from("fmt"), Value::from(fmt)),
            (Value::from("attStmt"), Value::Map(vec![])),
            (Value::from("authData"), Value::Bytes(auth_data)),
        ]))
    }

    fn registration(attestation_object: Vec<u8>, client_data_json: Vec<u8>) -> RootRegistration {
        RootRegistration::WebAuthn {
            attestation_object,
            client_data_json,
        }
    }

    fn good_registration(key: &SigningKey) -> RootRegistration {
        registration(
            attestation(
                "none",
                registration_data(PORTABLE_RP, 0x45, &[1; 32], &cbor(&cose_key(key))),
            ),
            client_data(WEBAUTHN_CREATE, &HANDLE),
        )
    }

    #[test]
    fn a_webauthn_registration_gives_the_credential_s_root() {
        let key = signing(3);
        assert_eq!(good_registration(&key).root(&HANDLE), Ok(root(&key)));
    }

    #[test]
    fn a_ctap_registration_gives_its_root_as_it_is() {
        let key = signing(3);
        assert_eq!(
            RootRegistration::Ctap(root(&key)).root(&HANDLE),
            Ok(root(&key))
        );
    }

    /// The attestation statement is not checked, whatever its format.
    #[test]
    fn a_webauthn_registration_s_attestation_statement_is_not_checked() {
        use ciborium::Value;
        let key = signing(3);
        let object = cbor(&Value::Map(vec![
            (Value::from("fmt"), Value::from("packed")),
            (
                Value::from("attStmt"),
                Value::Map(vec![
                    (Value::from("alg"), Value::from(-7)),
                    (Value::from("sig"), Value::Bytes(vec![0; 70])),
                ]),
            ),
            (
                Value::from("authData"),
                Value::Bytes(registration_data(
                    PORTABLE_RP,
                    0x45,
                    &[1; 32],
                    &cbor(&cose_key(&key)),
                )),
            ),
        ]));
        assert_eq!(
            registration(object, client_data(WEBAUTHN_CREATE, &HANDLE)).root(&HANDLE),
            Ok(root(&key))
        );
    }

    #[test]
    fn a_webauthn_get_presented_as_a_registration_is_refused() {
        let key = signing(3);
        let RootRegistration::WebAuthn {
            attestation_object, ..
        } = good_registration(&key)
        else {
            unreachable!()
        };
        let made = registration(attestation_object, client_data(WEBAUTHN_GET, &HANDLE));
        assert_eq!(made.root(&HANDLE), Err(WebAuthnError::WrongType));
    }

    #[test]
    fn a_webauthn_registration_for_another_handle_is_refused() {
        let key = signing(3);
        assert_eq!(
            good_registration(&key).root(&[0x43; HANDLE_LEN]),
            Err(WebAuthnError::WrongChallenge)
        );
    }

    #[test]
    fn a_webauthn_registration_refuses_another_rp_no_up_or_uv_and_no_credential() {
        let key = signing(3);
        let cose = cbor(&cose_key(&key));
        for (rp, flags, refusal) in [
            ("spoolframe.app", 0x45, WebAuthnError::WrongRp),
            (PORTABLE_RP, 0x41, WebAuthnError::NotVerified),
            (PORTABLE_RP, 0x44, WebAuthnError::NotVerified),
            (PORTABLE_RP, 0x05, WebAuthnError::NoCredential),
        ] {
            let data = if flags & FLAG_AT == 0 {
                authenticator_data(rp, flags)
            } else {
                registration_data(rp, flags, &[1; 32], &cose)
            };
            let made = registration(
                attestation("none", data),
                client_data(WEBAUTHN_CREATE, &HANDLE),
            );
            assert_eq!(made.root(&HANDLE), Err(refusal), "{rp} {flags:#x}");
        }
    }

    #[test]
    fn a_webauthn_registration_of_a_key_that_is_not_es256_p256_is_refused() {
        use ciborium::Value;
        let key = signing(3);
        let with = |at: usize, value: Value| {
            let Value::Map(mut members) = cose_key(&key) else {
                unreachable!()
            };
            members[at].1 = value;
            Value::Map(members)
        };
        let off_curve = {
            let Value::Map(mut members) = cose_key(&key) else {
                unreachable!()
            };
            members[4].1 = Value::Bytes(vec![0; 32]);
            Value::Map(members)
        };
        for cose in [
            with(0, Value::from(1)),  // OKP
            with(1, Value::from(-8)), // EdDSA
            with(2, Value::from(2)),  // P-384
            with(3, Value::Bytes(vec![1; 31])),
            off_curve,
        ] {
            let made = registration(
                attestation(
                    "none",
                    registration_data(PORTABLE_RP, 0x45, &[1; 32], &cbor(&cose)),
                ),
                client_data(WEBAUTHN_CREATE, &HANDLE),
            );
            assert_eq!(
                made.root(&HANDLE),
                Err(WebAuthnError::UnsupportedKey),
                "{cose:?}"
            );
        }
    }

    #[test]
    fn a_webauthn_registration_with_an_empty_or_long_credential_id_is_refused() {
        let key = signing(3);
        let cose = cbor(&cose_key(&key));
        for id in [vec![], vec![1; MAX_CREDENTIAL_ID + 1]] {
            let made = registration(
                attestation("none", registration_data(PORTABLE_RP, 0x45, &id, &cose)),
                client_data(WEBAUTHN_CREATE, &HANDLE),
            );
            assert_eq!(made.root(&HANDLE), Err(WebAuthnError::BadCredentialId));
        }
    }

    #[test]
    fn an_oversize_webauthn_registration_is_refused_before_it_parses() {
        let key = signing(3);
        let made = registration(
            vec![0; MAX_ATTESTATION_OBJECT + 1],
            client_data(WEBAUTHN_CREATE, &HANDLE),
        );
        assert_eq!(made.root(&HANDLE), Err(WebAuthnError::TooLong));
        let RootRegistration::WebAuthn {
            attestation_object, ..
        } = good_registration(&key)
        else {
            unreachable!()
        };
        let made = registration(attestation_object, vec![b' '; MAX_CLIENT_DATA_JSON + 1]);
        assert_eq!(made.root(&HANDLE), Err(WebAuthnError::TooLong));
    }

    #[test]
    fn a_malformed_attestation_object_is_refused() {
        use ciborium::Value;
        let key = signing(3);
        let cose = cbor(&cose_key(&key));
        let data = registration_data(PORTABLE_RP, 0x45, &[1; 32], &cose);
        let mut trailing = attestation("none", data.clone());
        trailing.push(0);
        let mut nested = Value::Bytes(data.clone());
        for _ in 0..1000 {
            nested = Value::Array(vec![nested]);
        }
        for object in [
            vec![],
            vec![0xff],
            b"not cbor".to_vec(),
            trailing,
            cbor(&Value::Array(vec![])),
            cbor(&nested),
            // No authData; authData not bytes; fmt not text; a key twice.
            cbor(&Value::Map(vec![
                (Value::from("fmt"), Value::from("none")),
                (Value::from("attStmt"), Value::Map(vec![])),
            ])),
            cbor(&Value::Map(vec![
                (Value::from("fmt"), Value::from("none")),
                (Value::from("attStmt"), Value::Map(vec![])),
                (Value::from("authData"), Value::from("text")),
            ])),
            cbor(&Value::Map(vec![
                (Value::from("fmt"), Value::from(1)),
                (Value::from("attStmt"), Value::Map(vec![])),
                (Value::from("authData"), Value::Bytes(data.clone())),
            ])),
            cbor(&Value::Map(vec![
                (Value::from("fmt"), Value::from("none")),
                (Value::from("attStmt"), Value::Map(vec![])),
                (Value::from("authData"), Value::Bytes(data.clone())),
                (Value::from("authData"), Value::Bytes(data.clone())),
            ])),
        ] {
            let made = registration(object.clone(), client_data(WEBAUTHN_CREATE, &HANDLE));
            assert_eq!(
                made.root(&HANDLE),
                Err(WebAuthnError::MalformedAttestation),
                "{object:02x?}"
            );
        }
    }

    #[test]
    fn malformed_attested_credential_data_is_refused() {
        let key = signing(3);
        let cose = cbor(&cose_key(&key));
        let data = registration_data(PORTABLE_RP, 0x45, &[1; 32], &cose);
        let mut trailing = data.clone();
        trailing.push(0);
        let mut cose_twice = registration_data(PORTABLE_RP, 0x45, &[1; 32], &[]);
        let ciborium::Value::Map(mut members) = cose_key(&key) else {
            unreachable!()
        };
        members.push(members[0].clone());
        cose_twice.extend_from_slice(&cbor(&ciborium::Value::Map(members)));
        for data in [
            data[..37 + 10].to_vec(),
            data[..37 + 18 + 10].to_vec(),
            data[..data.len() - 1].to_vec(),
            // Bytes after the key, with no extensions flagged.
            trailing,
            cose_twice,
        ] {
            let made = registration(
                attestation("none", data),
                client_data(WEBAUTHN_CREATE, &HANDLE),
            );
            assert_eq!(
                made.root(&HANDLE),
                Err(WebAuthnError::MalformedAuthenticatorData)
            );
        }
        // With extensions flagged, what follows the key is theirs.
        let mut extended = registration_data(PORTABLE_RP, 0xc5, &[1; 32], &cose);
        extended.extend_from_slice(&cbor(&ciborium::Value::Map(vec![])));
        let made = registration(
            attestation("none", extended),
            client_data(WEBAUTHN_CREATE, &HANDLE),
        );
        assert_eq!(made.root(&HANDLE), Ok(root(&key)));
    }
}
