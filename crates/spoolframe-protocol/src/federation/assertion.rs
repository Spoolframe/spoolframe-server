//! Delegated authentication (P8-03): the server a user is at home on (SB)
//! vouches for that user to the server whose item they want to play (SA),
//! and SA answers with a short-lived token for that one item.
//!
//! [`AssertUser`] arriving on SB's connection is the proof: the connection
//! proves SB's endpoint key, and only a live pair's connection reaches it.
//! There is no signature, and an assertion cannot be replayed on another
//! connection. SB asserts only its own local accounts, never a portable
//! guest of its, and names each by its stable user id, so a rename on SB
//! changes nothing on SA.
//!
//! SA checks that the item is in a library shared with SB, and mints a
//! random bearer token, bound to the device's endpoint id: a token that
//! leaks is useless from another device. P8-04 redeems it on SA's client
//! protocol.

use std::fmt;

use serde::{Deserialize, Serialize};

/// What a token opens. A play token opens one item's playback; P8-07 adds
/// a download token, which opens one item's download.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenScope {
    #[default]
    Play,
    /// A scope this build does not know: refused.
    #[serde(other)]
    Unknown,
}

impl TokenScope {
    /// Its name on the wire, and in the store.
    pub fn name(self) -> &'static str {
        match self {
            TokenScope::Play => "play",
            TokenScope::Unknown => "unknown",
        }
    }
}

/// SB vouches for its local `user`, who wants `item` of SA's on the
/// device `client_endpoint`: `Token`, or `AssertionRefused`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssertUser {
    /// The user's stable id on SB: a UUID, never a portable user's id.
    pub user: String,
    /// What SB shows them as: for SA's logs, cut to
    /// [`crate::MAX_HELLO_TEXT`] characters. SA's admin pages never list
    /// it.
    pub display_name: String,
    /// The device's endpoint id, hex: the only one the token works from.
    pub client_endpoint: String,
    /// The item, by SA's own id (what SB's `fed:` id carries).
    pub item: String,
    /// What the token is for; a play when left out.
    #[serde(default)]
    pub scope: TokenScope,
}

impl AssertUser {
    /// This assertion with its name cut to [`crate::MAX_HELLO_TEXT`]
    /// characters: what a server reads of it.
    pub fn within_bounds(mut self) -> Self {
        self.display_name = super::server_name(self.display_name);
        self
    }
}

/// SA's token for an [`AssertUser`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenGranted {
    /// The bearer token: whoever holds it, from the device it is bound to,
    /// may do what its scope says with the item, once.
    pub token: String,
    pub scope: TokenScope,
    /// When it stops working, Unix seconds: minutes from now.
    pub expires_at: i64,
}

impl fmt::Debug for TokenGranted {
    // The token opens an item until it expires: keep it out of logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenGranted")
            .field("token", &"<redacted>")
            .field("scope", &self.scope)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Why SA minted no token. Each is counted against the pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionRefused {
    /// The item is in no library shared with the pair, or is not SA's own.
    NotShared,
    /// The user is not named by a stable user id: a portable guest, or
    /// nothing that reads.
    InvalidUser,
    /// The device's endpoint id does not read.
    InvalidDevice,
    /// A scope SA does not know.
    InvalidScope,
    /// SA cannot say now (its catalog or store failed): try again later.
    Unavailable,
    /// A reason this build does not know.
    #[serde(other)]
    Unknown,
}

impl fmt::Display for AssertionRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AssertionRefused::NotShared => "the item is not shared with this server",
            AssertionRefused::InvalidUser => "that is not a user the server may vouch for",
            AssertionRefused::InvalidDevice => "the device's id does not read",
            AssertionRefused::InvalidScope => "a token for that is not made",
            AssertionRefused::Unavailable => "the other server cannot answer now",
            AssertionRefused::Unknown => "refused for a reason this build does not know",
        })
    }
}
