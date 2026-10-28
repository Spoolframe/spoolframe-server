//! Delegated authentication: the user's home server vouches for the user
//! to the item's server (the paired server whose item they want to play),
//! and the item's server answers with a short-lived token for that one
//! item.
//!
//! [`AssertUser`] arriving on the home server's connection is the proof:
//! the connection proves the home server's endpoint key, and only a live
//! pair's connection reaches it. There is no signature, and an assertion
//! cannot be replayed on another connection. The home server asserts only
//! its own local accounts, never a portable guest of its, and names each
//! by its stable user id, so a rename on the home server changes nothing
//! on the item's server.
//!
//! The item's server checks that the item is in a library shared with the
//! home server, and mints a random bearer token, bound to the device's
//! endpoint id: a token that leaks is useless from another device. The
//! token is redeemed on the item's server's client protocol
//! (`Request::RedeemPlayToken`).

use std::fmt;

use serde::{Deserialize, Serialize};

/// What a token opens. A play token opens one item's playback; a
/// download token opens one item's download.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenScope {
    #[default]
    Play,
    /// One item's download: preparing it, asking how it goes,
    /// cancelling it, reading its file and hearing `DownloadChanged`. The
    /// session it opens lives while the download is made and read, not
    /// minutes as a play's.
    Download,
    /// A scope this build does not know: refused.
    #[serde(other)]
    Unknown,
}

impl TokenScope {
    /// Its name on the wire.
    pub fn name(self) -> &'static str {
        match self {
            TokenScope::Play => "play",
            TokenScope::Download => "download",
            TokenScope::Unknown => "unknown",
        }
    }
}

/// The home server vouches for its local `user`, who wants `item` of the
/// item's server on the device `client_endpoint`. Answered with `Token`, or
/// `AssertionRefused`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssertUser {
    /// The user's stable id on the home server: a UUID, never a portable
    /// user's id.
    pub user: String,
    /// Always empty: a server never sends its users' names to another,
    /// which knows them by `user` alone. Kept for the wire; what a server
    /// reads of it is cut to [`crate::MAX_HELLO_TEXT`] characters.
    pub display_name: String,
    /// The device's endpoint id, hex: the only one the token works from.
    pub client_endpoint: String,
    /// The item, by the item's server's own id (what the home server's
    /// `fed:` id carries).
    pub item: String,
    /// What the token is for; a play when left out.
    #[serde(default)]
    pub scope: TokenScope,
    /// The user's track preferences on the home server: what the item's
    /// server chooses the audio and subtitles by when the play sends no
    /// selection, as the home server would. Left out, the item's server
    /// uses its defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferences: Option<spoolframe_types::TrackPreferences>,
    /// A download token's one download, by the item's server's id: what
    /// the device's own token reads. Left out, the token's session
    /// prepares the item's download itself (the home server's own
    /// download, which keeps the record).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download: Option<String>,
}

impl AssertUser {
    /// This assertion with its name cut to [`crate::MAX_HELLO_TEXT`]
    /// characters: what a server reads of it.
    pub fn within_bounds(mut self) -> Self {
        self.display_name = super::server_name(self.display_name);
        self
    }
}

/// The item's server's token for an [`AssertUser`].
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

/// Why the item's server minted no token. Each is counted against the
/// pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionRefused {
    /// The item is in no library shared with the pair, or is not the
    /// item's server's own.
    NotShared,
    /// The user is not named by a stable user id: a portable guest, or
    /// nothing that reads.
    InvalidUser,
    /// The device's endpoint id does not read.
    InvalidDevice,
    /// A scope the item's server does not know.
    InvalidScope,
    /// The item's server cannot say now (its catalog or store failed):
    /// try again later.
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
