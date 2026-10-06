//! Passkeys in the apps (P3-19), through one relying party every server
//! shares: [`APP_RP_ID`].
//!
//! A platform's passkey API works only for a domain the app is associated
//! with when it is built; a self-hosted server's own domain is not known
//! then. So the apps make and use their passkeys for `spoolframe.app`,
//! whose only part in this is the association file it serves to the
//! operating system. Sign-in runs between the app and the server over
//! this protocol; the server never contacts `spoolframe.app`.
//!
//! Every server keeps its own passkeys, so a passkey made on one is
//! refused by another. Each user has a random user handle per server, so
//! one server's says nothing about the same person on another.
//!
//! The platform writes only the origin and the challenge into
//! `clientDataJSON`, and the origin is the same for every server. So the
//! challenge names the server (PX-09, finding 2): a server sends a random
//! nonce, never a challenge, and the challenge is [`app_passkey_challenge`]
//! of the server's endpoint id and that nonce. The server computes it with
//! its own id; the client with the id it pinned and dialled, never one the
//! server names. A server that hands on another's nonce, to relay a
//! sign-in, has the app sign a challenge for itself, which the other
//! refuses.
//!
//! The bytes are WebAuthn's own, as the platform API gives and takes them;
//! the app maps them to and from it and changes nothing.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The relying-party ID of app passkeys: what the app asks its platform's
/// passkey API for, on every server.
pub const APP_RP_ID: &str = "spoolframe.app";

/// The origin the platform writes into `clientDataJSON` for
/// [`APP_RP_ID`]; the server accepts no other for app passkeys.
pub const APP_ORIGIN: &str = "https://spoolframe.app";

/// Bytes of a nonce a server sends.
pub const NONCE_LEN: usize = 32;

/// What [`app_passkey_challenge`] hashes first, so its digest is of use
/// for nothing else.
const CHALLENGE_TAG: &[u8] = b"spoolframe/app-passkey-challenge/1";

/// The WebAuthn challenge for an app passkey ceremony with the server
/// whose endpoint id is `server`, from the `nonce` it sent: SHA-256 of
/// [`CHALLENGE_TAG`], the 32 bytes of the id, then the nonce. The tag and
/// the id are of fixed length, so nothing else hashes the same.
pub fn app_passkey_challenge(server: &[u8; 32], nonce: &[u8]) -> Vec<u8> {
    Sha256::new()
        .chain_update(CHALLENGE_TAG)
        .chain_update(server)
        .chain_update(nonce)
        .finalize()
        .to_vec()
}

/// A server's answer to `BeginPasskeyRegistration`, on the wire: a nonce,
/// not a challenge. [`PasskeyRegistrationBegun::for_server`] makes the
/// [`PasskeyCreation`] the platform is given.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasskeyRegistrationBegun {
    /// Of use once, for 5 minutes, from this device.
    #[serde(with = "serde_bytes")]
    pub nonce: Vec<u8>,
    /// The user handle: random, this server's for this user.
    #[serde(with = "serde_bytes")]
    pub user_id: Vec<u8>,
    /// As [`PasskeyCreation::user_name`].
    pub user_name: String,
    /// As [`PasskeyCreation::display_name`].
    pub display_name: String,
    /// As [`PasskeyCreation::exclude_credential_ids`].
    #[serde(with = "byte_strings")]
    pub exclude_credential_ids: Vec<Vec<u8>>,
}

impl PasskeyRegistrationBegun {
    /// What the platform makes the passkey with, for the server whose
    /// endpoint id is `server`: the one the client dialled.
    pub fn for_server(self, server: &[u8; 32]) -> PasskeyCreation {
        PasskeyCreation {
            challenge: app_passkey_challenge(server, &self.nonce),
            user_id: self.user_id,
            user_name: self.user_name,
            display_name: self.display_name,
            exclude_credential_ids: self.exclude_credential_ids,
        }
    }
}

/// A server's answer to `BeginPasskeySignIn` and `BeginConfirmPasskey`,
/// on the wire: a nonce, not a challenge.
/// [`PasskeySignInBegun::for_server`] makes the [`PasskeyRequest`] the
/// platform is given.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasskeySignInBegun {
    /// Of use once, for 5 minutes, from this device.
    #[serde(with = "serde_bytes")]
    pub nonce: Vec<u8>,
    /// As [`PasskeyRequest::allow_credential_ids`]. Always empty for
    /// `BeginPasskeySignIn`: anyone can begin one.
    #[serde(with = "byte_strings")]
    pub allow_credential_ids: Vec<Vec<u8>>,
}

impl PasskeySignInBegun {
    /// What the platform signs, for the server whose endpoint id is
    /// `server`: the one the client dialled.
    pub fn for_server(self, server: &[u8; 32]) -> PasskeyRequest {
        PasskeyRequest {
            challenge: app_passkey_challenge(server, &self.nonce),
            allow_credential_ids: self.allow_credential_ids,
        }
    }
}

/// What making a passkey needs: the platform's options. The relying party
/// is [`APP_RP_ID`]; user verification is required. Not on the wire: the
/// client makes it from a [`PasskeyRegistrationBegun`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasskeyCreation {
    /// To sign: [`app_passkey_challenge`] of the server and its nonce.
    #[serde(with = "serde_bytes")]
    pub challenge: Vec<u8>,
    /// The user handle: random, this server's for this user.
    #[serde(with = "serde_bytes")]
    pub user_id: Vec<u8>,
    /// The account's name in the platform's passkey list,
    /// `<username> on <server name>`: one keychain may hold passkeys of
    /// several servers, all for [`APP_RP_ID`].
    pub user_name: String,
    /// `<display name> on <server name>`.
    pub display_name: String,
    /// The user's app passkeys on this server: an authenticator holding
    /// one of these is not to make another.
    #[serde(with = "byte_strings")]
    pub exclude_credential_ids: Vec<Vec<u8>>,
}

/// A passkey made for a [`PasskeyCreation`], to keep
/// (`FinishPasskeyRegistration`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishPasskeyRegistration {
    #[serde(with = "serde_bytes")]
    pub credential_id: Vec<u8>,
    /// The attestation object (CBOR), as the platform gave it.
    #[serde(with = "serde_bytes")]
    pub attestation_object: Vec<u8>,
    /// `clientDataJSON`, as the platform gave it.
    #[serde(with = "serde_bytes")]
    pub client_data_json: Vec<u8>,
    /// What the device calls itself; the user's profile shows the passkey
    /// as "Spoolframe app · <name>". 1 to 64 characters.
    pub name: String,
}

/// What signing in with a passkey needs: the platform's options. Not on
/// the wire: the client makes it from a [`PasskeySignInBegun`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasskeyRequest {
    /// To sign: [`app_passkey_challenge`] of the server and its nonce.
    #[serde(with = "serde_bytes")]
    pub challenge: Vec<u8>,
    /// The passkeys that may answer; empty: any the platform finds for
    /// [`APP_RP_ID`] (a discoverable sign-in).
    #[serde(with = "byte_strings")]
    pub allow_credential_ids: Vec<Vec<u8>>,
}

/// A passkey's answer to a [`PasskeyRequest`] (`FinishPasskeySignIn`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishPasskeySignIn {
    #[serde(with = "serde_bytes")]
    pub credential_id: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub authenticator_data: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub client_data_json: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub signature: Vec<u8>,
    /// The user handle the authenticator keeps with the passkey, or
    /// `None` when the platform gave none (webauthn-authenticator-rs, for
    /// a phone over caBLE). Never `Some` of no bytes: the server refuses
    /// that as `InvalidInput`. The client SDK sends an empty handle as
    /// `None`. On the wire a CBOR byte string, or null for `None`; the key
    /// is always there.
    #[serde(with = "optional_bytes")]
    pub user_handle: Option<Vec<u8>>,
}

/// One of the session user's passkeys (`ListPasskeys`), the web UI's as
/// well as the apps': the user sees every way they sign in, and the web
/// UI's `me.passkeys` lists the same ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasskeyInfo {
    /// Its credential id, base64url with no padding, as the web UI names
    /// it: what `RenamePasskey` and `RemovePasskey` take. For an app
    /// passkey, that of the `credential_id` bytes the platform gave.
    pub id: String,
    /// What its user called it; an app passkey's is the device's (the web
    /// UI shows it as "Spoolframe app · <name>").
    pub name: String,
    /// Unix seconds.
    pub created_at: i64,
    /// `None` if it has never signed them in.
    pub last_used_at: Option<i64>,
    /// The relying party it was made for: [`APP_RP_ID`], or the host of
    /// the server's web UI.
    pub rp_id: String,
    /// Made in a Spoolframe app, for [`APP_RP_ID`].
    pub app: bool,
    /// An app can sign in with it: only an app passkey. A web UI's is
    /// listed so the user sees it, and can rename or remove it.
    pub usable_here: bool,
}

// Byte fields are shown by length: they are not secrets, but a log of
// them says nothing and takes a screen.

impl fmt::Debug for PasskeyCreation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasskeyCreation")
            .field("user_name", &self.user_name)
            .field("display_name", &self.display_name)
            .field("exclude", &self.exclude_credential_ids.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for PasskeyRegistrationBegun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasskeyRegistrationBegun")
            .field("user_name", &self.user_name)
            .field("display_name", &self.display_name)
            .field("exclude", &self.exclude_credential_ids.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for PasskeySignInBegun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasskeySignInBegun")
            .field("allow", &self.allow_credential_ids.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for FinishPasskeyRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FinishPasskeyRegistration")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for PasskeyRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasskeyRequest")
            .field("allow", &self.allow_credential_ids.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for FinishPasskeySignIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FinishPasskeySignIn")
            .field("user_handle", &self.user_handle.as_ref().map(Vec::len))
            .finish_non_exhaustive()
    }
}

/// A list of byte strings, each a CBOR byte string as `serde_bytes` makes
/// one (it has no impl for a list of them).
mod byte_strings {
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_bytes::{ByteBuf, Bytes};

    pub fn serialize<S: Serializer>(list: &[Vec<u8>], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(list.iter().map(|bytes| Bytes::new(bytes)))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Vec<u8>>, D::Error> {
        let list = Vec::<ByteBuf>::deserialize(deserializer)?;
        Ok(list.into_iter().map(ByteBuf::into_vec).collect())
    }
}

/// An optional byte string: CBOR bytes, or null. Its field is required:
/// `with` makes serde ask for it, as it does not for a plain `Option`.
mod optional_bytes {
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_bytes::{ByteBuf, Bytes};

    pub fn serialize<S: Serializer>(
        bytes: &Option<Vec<u8>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(bytes) => serializer.serialize_some(Bytes::new(bytes)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Vec<u8>>, D::Error> {
        Ok(Option::<ByteBuf>::deserialize(deserializer)?.map(ByteBuf::into_vec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: [u8; NONCE_LEN] = [7; NONCE_LEN];

    #[test]
    fn the_challenge_differs_for_another_server_with_the_same_nonce() {
        assert_ne!(
            app_passkey_challenge(&[1; 32], &NONCE),
            app_passkey_challenge(&[2; 32], &NONCE)
        );
    }

    #[test]
    fn the_challenge_is_the_tagged_sha256_of_the_server_and_nonce() {
        let mut bytes = b"spoolframe/app-passkey-challenge/1".to_vec();
        bytes.extend([1; 32]);
        bytes.extend(NONCE);
        assert_eq!(
            app_passkey_challenge(&[1; 32], &NONCE),
            Sha256::digest(&bytes).to_vec()
        );
    }

    #[test]
    fn a_request_for_a_server_signs_its_challenge_and_keeps_the_list() {
        let begun = PasskeySignInBegun {
            nonce: NONCE.to_vec(),
            allow_credential_ids: vec![vec![4]],
        };
        assert_eq!(
            begun.for_server(&[1; 32]),
            PasskeyRequest {
                challenge: app_passkey_challenge(&[1; 32], &NONCE),
                allow_credential_ids: vec![vec![4]],
            }
        );
    }
}
