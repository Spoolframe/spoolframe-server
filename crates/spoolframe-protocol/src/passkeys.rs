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
//! Every server keeps its own passkeys and issues its own challenges, so a
//! passkey made on one is refused by another, and an assertion made for
//! one cannot be replayed to another. Each user has a random user handle
//! per server, so one server's says nothing about the same person on
//! another.
//!
//! The bytes are WebAuthn's own, as the platform API gives and takes them;
//! the app maps them to and from it and changes nothing.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The relying-party ID of app passkeys: what the app asks its platform's
/// passkey API for, on every server.
pub const APP_RP_ID: &str = "spoolframe.app";

/// The origin the platform writes into `clientDataJSON` for
/// [`APP_RP_ID`]; the server accepts no other for app passkeys.
pub const APP_ORIGIN: &str = "https://spoolframe.app";

/// What making a passkey needs (`BeginPasskeyRegistration`). The relying
/// party is [`APP_RP_ID`]; user verification is required.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasskeyCreation {
    /// To sign; of use once, for 5 minutes, from this device.
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

/// What signing in with a passkey needs (`BeginPasskeySignIn`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasskeyRequest {
    /// To sign; of use once, for 5 minutes, from this device.
    #[serde(with = "serde_bytes")]
    pub challenge: Vec<u8>,
    /// The passkeys that may answer; empty: any of this server's the
    /// platform finds (a discoverable sign-in, with no username).
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
    /// The user handle the authenticator keeps with the passkey.
    #[serde(with = "serde_bytes")]
    pub user_handle: Vec<u8>,
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
