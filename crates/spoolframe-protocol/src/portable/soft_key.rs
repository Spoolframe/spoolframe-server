//! A software security key: a P-256 key that answers as a FIDO2
//! key's resident credential does, writing real CTAP2 assertions under
//! [`PORTABLE_RP`]. For tests, and for trying key roots with no key at
//! hand; never a root anyone relies on, its secret being in memory.
//!
//! It mirrors what the spike read off the user's Feitian key through
//! libfido2 (`__misc/p7-08-key-spike`): a 32-byte credential id, an
//! assertion's flags UP and UV (0x05) with the counter 0. UV is the PIN's;
//! there is no "uv" option to pass.
//!
//! It answers in either form a root takes: raw CTAP2, as libfido2 does,
//! or WebAuthn, as Apple's system security-key API does (a
//! `clientDataJSON` around the challenge, an `attestationObject` of
//! format `none`). The same key in both forms is one credential, so a
//! test registers it one way and signs the other.

use p256::ecdsa::signature::Signer;
use p256::ecdsa::{DerSignature, SigningKey};
use sha2::{Digest, Sha256};

use super::roots::{
    HANDLE_LEN, P256_PUBLIC_KEY_LEN, PORTABLE_RP, RootKey, RootRegistration, RootSignature,
};

/// `authenticatorData` flags of an assertion made with a touch and the PIN.
pub const UP_UV: u8 = 0x05;

/// `authenticatorData` flags of a registration: UP, UV and attested
/// credential data.
pub const UP_UV_AT: u8 = 0x45;

/// The origin Apple's API writes for an app entitled to [`PORTABLE_RP`].
pub const ORIGIN: &str = "https://portable.spoolframe.app";

/// A `clientDataJSON` as a platform writes it: `type`, `challenge`
/// (base64url, no padding), `origin`, `crossOrigin` false.
pub fn client_data_json(kind: &str, challenge: &[u8]) -> Vec<u8> {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        r#"{{"type":"{kind}","challenge":"{}","origin":"{ORIGIN}","crossOrigin":false}}"#,
        URL_SAFE_NO_PAD.encode(challenge)
    )
    .into_bytes()
}

/// A software key holding one credential.
#[derive(Clone)]
pub struct SoftKey {
    signing: SigningKey,
    credential_id: Vec<u8>,
    /// The resident credential's `user.id`: the identity's handle.
    user_handle: Option<[u8; 32]>,
}

impl SoftKey {
    /// The key whose secret scalar is `seed` repeated: the same in every
    /// run, for vectors.
    pub fn from_seed(seed: u8) -> Self {
        let signing = SigningKey::from_bytes(&[seed.max(1); 32].into())
            .expect("a small repeated byte is a valid P-256 scalar");
        Self {
            signing,
            credential_id: vec![seed; 32],
            user_handle: None,
        }
    }

    /// A new key from `random`, 32 bytes the caller drew (and a credential
    /// id from the next 32, the length the user's Feitian key gives).
    /// `None` for a draw that is not a scalar, about one in 2^32.
    pub fn from_random(random: [u8; 64]) -> Option<Self> {
        let mut scalar = [0u8; 32];
        scalar.copy_from_slice(&random[..32]);
        let signing = SigningKey::from_bytes(&scalar.into()).ok()?;
        Some(Self {
            signing,
            credential_id: random[32..].to_vec(),
            user_handle: None,
        })
    }

    /// The same key, its credential made for the identity `handle`.
    pub fn with_handle(mut self, handle: [u8; 32]) -> Self {
        self.user_handle = Some(handle);
        self
    }

    /// The handle its credential was made for, if any.
    pub fn user_handle(&self) -> Option<[u8; 32]> {
        self.user_handle
    }

    pub fn credential_id(&self) -> &[u8] {
        &self.credential_id
    }

    /// Its root.
    pub fn root(&self) -> RootKey {
        let point = self.signing.verifying_key().to_sec1_point(false);
        let public_key: [u8; P256_PUBLIC_KEY_LEN] = point
            .as_bytes()
            .try_into()
            .expect("an uncompressed P-256 point is 65 bytes");
        RootKey::SecurityKey {
            public_key,
            credential_id: self.credential_id.clone(),
        }
    }

    /// Its assertion over `message`, touched and with the PIN: what a
    /// FIDO2 key returns for `clientDataHash` = SHA-256(`message`).
    pub fn assert(&self, message: &[u8]) -> RootSignature {
        self.assert_with_flags(message, UP_UV)
    }

    /// Its assertion over `message` with `flags` in its
    /// `authenticatorData`: for refusals.
    pub fn assert_with_flags(&self, message: &[u8], flags: u8) -> RootSignature {
        self.assert_hash_with_flags(Sha256::digest(message).into(), flags)
    }

    /// Its assertion over a `clientDataHash` the caller made, as a key is
    /// asked: touched and with the PIN.
    pub fn assert_prehashed(&self, client_data_hash: [u8; 32]) -> RootSignature {
        self.assert_hash_with_flags(client_data_hash, UP_UV)
    }

    fn assert_hash_with_flags(&self, client_data_hash: [u8; 32], flags: u8) -> RootSignature {
        let authenticator_data = authenticator_data(flags);
        RootSignature::Ctap {
            signature: self.sign(&authenticator_data, &client_data_hash),
            authenticator_data,
        }
    }

    /// `authenticator_data ‖ client_data_hash`, signed: what a key signs
    /// in either form.
    fn sign(&self, authenticator_data: &[u8], client_data_hash: &[u8]) -> Vec<u8> {
        let mut signed = authenticator_data.to_vec();
        signed.extend_from_slice(client_data_hash);
        let signature: DerSignature = self.signing.sign(&signed);
        signature.as_bytes().to_vec()
    }

    /// Its WebAuthn assertion over `message`, touched and with the PIN:
    /// what Apple's API returns for `challenge` = SHA-256(`message`).
    pub fn assert_webauthn(&self, message: &[u8]) -> RootSignature {
        self.assert_webauthn_prehashed(Sha256::digest(message).into())
    }

    /// Its WebAuthn assertion for a `challenge` the caller made, as the
    /// SDK asks a key: the SHA-256 of the message.
    pub fn assert_webauthn_prehashed(&self, challenge: [u8; 32]) -> RootSignature {
        self.assert_webauthn_with(client_data_json("webauthn.get", &challenge), UP_UV)
    }

    /// Its WebAuthn assertion with `client_data_json` and `flags` as
    /// given, signed as a key signs: for refusals.
    pub fn assert_webauthn_with(&self, client_data_json: Vec<u8>, flags: u8) -> RootSignature {
        let authenticator_data = authenticator_data(flags);
        RootSignature::WebAuthn {
            signature: self.sign(&authenticator_data, &Sha256::digest(&client_data_json)),
            authenticator_data,
            client_data_json,
        }
    }

    /// Its credential as libfido2 reports it: the root itself.
    pub fn register_ctap(&self) -> RootRegistration {
        RootRegistration::Ctap(self.root())
    }

    /// Its credential as Apple's API reports it, made for the identity
    /// `handle` (the challenge): an `attestationObject` of format `none`
    /// whose `authData` carries the credential id and its COSE ES256 key.
    pub fn register_webauthn(&self, handle: &[u8; HANDLE_LEN]) -> RootRegistration {
        use ciborium::Value;
        let point = self.signing.verifying_key().to_sec1_point(false);
        let cose = Value::Map(vec![
            (Value::from(1), Value::from(2)),
            (Value::from(3), Value::from(-7)),
            (Value::from(-1), Value::from(1)),
            (
                Value::from(-2),
                Value::Bytes(point.x().expect("an uncompressed point").to_vec()),
            ),
            (
                Value::from(-3),
                Value::Bytes(point.y().expect("an uncompressed point").to_vec()),
            ),
        ]);
        let mut auth_data = authenticator_data(UP_UV_AT);
        auth_data.extend_from_slice(&[0; 16]);
        auth_data.extend_from_slice(
            &u16::try_from(self.credential_id.len())
                .expect("a credential id fits a u16")
                .to_be_bytes(),
        );
        auth_data.extend_from_slice(&self.credential_id);
        ciborium::into_writer(&cose, &mut auth_data).expect("CBOR writes to a Vec");
        let object = Value::Map(vec![
            (Value::from("fmt"), Value::from("none")),
            (Value::from("attStmt"), Value::Map(vec![])),
            (Value::from("authData"), Value::Bytes(auth_data)),
        ]);
        let mut attestation_object = Vec::new();
        ciborium::into_writer(&object, &mut attestation_object).expect("CBOR writes to a Vec");
        RootRegistration::WebAuthn {
            attestation_object,
            client_data_json: client_data_json("webauthn.create", handle),
        }
    }
}

/// `authenticatorData` under [`PORTABLE_RP`] with `flags` and counter 0.
fn authenticator_data(flags: u8) -> Vec<u8> {
    let mut authenticator_data = Sha256::digest(PORTABLE_RP.as_bytes()).to_vec();
    authenticator_data.push(flags);
    authenticator_data.extend_from_slice(&0u32.to_be_bytes());
    authenticator_data
}

impl std::fmt::Debug for SoftKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SoftKey({:?})", self.root())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HANDLE: [u8; 32] = [0x42; 32];

    /// One credential, two forms: registered through either, it signs
    /// through either, and both verify against the same root.
    #[test]
    fn its_ctap_and_webauthn_forms_are_one_credential() {
        let key = SoftKey::from_seed(3);
        let by_ctap = key.register_ctap().root(&HANDLE).unwrap();
        let by_webauthn = key.register_webauthn(&HANDLE).root(&HANDLE).unwrap();
        assert_eq!(by_ctap, key.root());
        assert_eq!(by_webauthn, key.root());
        for root in [by_ctap, by_webauthn] {
            assert!(root.verify(b"message", &key.assert(b"message")));
            assert!(root.verify(b"message", &key.assert_webauthn(b"message")));
            assert!(!root.verify(b"other", &key.assert_webauthn(b"message")));
        }
        assert!(matches!(
            key.assert_webauthn(b"message"),
            RootSignature::WebAuthn { .. }
        ));
    }

    #[test]
    fn its_webauthn_assertion_without_the_pin_or_of_the_wrong_type_does_not_verify() {
        let key = SoftKey::from_seed(3);
        let challenge: [u8; 32] = Sha256::digest(b"message").into();
        assert!(key.root().verify(
            b"message",
            &key.assert_webauthn_with(client_data_json("webauthn.get", &challenge), UP_UV)
        ));
        assert!(!key.root().verify(
            b"message",
            &key.assert_webauthn_with(client_data_json("webauthn.get", &challenge), 0x01)
        ));
        assert!(!key.root().verify(
            b"message",
            &key.assert_webauthn_with(client_data_json("webauthn.create", &challenge), UP_UV)
        ));
    }

    /// A genesis and an added root signed in mixed forms hold: the form a
    /// device signs in is its own business.
    #[test]
    fn a_genesis_and_an_addition_signed_in_mixed_forms_hold() {
        use crate::portable::{AddedRoot, IdentityGenesis, root_add_message, root_endorse_message};
        let (linux, iphone) = (SoftKey::from_seed(3), SoftKey::from_seed(4));
        let roots = vec![linux.root(), iphone.root()];
        let message = IdentityGenesis::message(&HANDLE, &roots).unwrap();
        let genesis = IdentityGenesis {
            handle: HANDLE,
            roots,
            signatures: vec![linux.assert(&message), iphone.assert_webauthn(&message)],
        };
        assert_eq!(genesis.verify(), Ok(()));
        let user = genesis.user();
        let added = SoftKey::from_seed(5);
        let addition = AddedRoot {
            root: added.root(),
            added_by: iphone.root(),
            added_at: 1_791_504_000,
            endorsement: iphone.assert_webauthn(&root_endorse_message(
                &user,
                &added.root(),
                1_791_504_000,
            )),
            possession: added.assert_webauthn(&root_add_message(&user, &added.root())),
        };
        assert!(addition.verify(&user));
        // The possession over another identity's addition does not.
        let other = AddedRoot {
            possession: added.assert_webauthn(&root_add_message(
                &crate::portable::PortableUserId([9; 32]),
                &added.root(),
            )),
            ..addition
        };
        assert!(!other.verify(&user));
    }

    #[test]
    fn its_assertion_verifies_and_without_the_pin_does_not() {
        let key = SoftKey::from_seed(3);
        assert!(key.root().is_valid());
        assert!(key.root().verify(b"message", &key.assert(b"message")));
        assert!(!key.root().verify(b"other", &key.assert(b"message")));
        assert!(
            !key.root()
                .verify(b"message", &key.assert_with_flags(b"message", 0x01))
        );
    }
}
