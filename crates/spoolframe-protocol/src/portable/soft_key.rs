//! A software security key (P7-08): a P-256 key that answers as a FIDO2
//! key's resident credential does, writing real CTAP2 assertions under
//! [`PORTABLE_RP`]. For tests, and for trying key roots with no key at
//! hand; never a root anyone relies on, its secret being in memory.
//!
//! It mirrors what the spike read off the user's Feitian key through
//! libfido2 (`__misc/p7-08-key-spike`): a 32-byte credential id, an
//! assertion's flags UP and UV (0x05) with the counter 0. UV is the PIN's;
//! there is no "uv" option to pass.

use p256::ecdsa::signature::Signer;
use p256::ecdsa::{DerSignature, SigningKey};
use sha2::{Digest, Sha256};

use super::roots::{P256_PUBLIC_KEY_LEN, PORTABLE_RP, RootKey, RootSignature};

/// `authenticatorData` flags of an assertion made with a touch and the PIN.
pub const UP_UV: u8 = 0x05;

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
        let point = self.signing.verifying_key().to_encoded_point(false);
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
        let mut authenticator_data = Sha256::digest(PORTABLE_RP.as_bytes()).to_vec();
        authenticator_data.push(flags);
        authenticator_data.extend_from_slice(&0u32.to_be_bytes());
        let mut signed = authenticator_data.clone();
        signed.extend_from_slice(&client_data_hash);
        let signature: DerSignature = self.signing.sign(&signed);
        RootSignature::Ctap {
            authenticator_data,
            signature: signature.as_bytes().to_vec(),
        }
    }
}

impl std::fmt::Debug for SoftKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SoftKey({:?})", self.root())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
