//! The media path (P8-04): what the client protocol carries for playing a
//! paired server's item.
//!
//! The device decides how it plays a paired server's (SA's) item, not the
//! user's home server (SB): it asks SB where SA is (`RemoteHints`), dials
//! SA with a short budget, and, reached, asks SB for a token for itself
//! (`RemoteGrant`). Not reached, it plays the item from SB (`StartPlayback`
//! of the `fed:` id), which plays from SA in its own name and on to the
//! device.
//!
//! SA minted the token for one of SB's users ([`super::TokenGranted`]).
//! Whoever plays
//! redeems it on SA's own `spoolframe/client/1`
//! (`Request::RedeemPlayToken`): the device itself when it can reach SA
//! (direct), or SB, which plays on to the device (proxy). Either way SA
//! answers with a session that opens that one item's playback and its
//! controls, and nothing else; `spoolframe/federation/1` carries no media.
//!
//! Experimental ([`crate::experimental::FEDERATION`]): used only by a
//! client that names the feature, without a `PROTOCOL_VERSION` bump.

use std::fmt;

use serde::{Deserialize, Serialize};
use spoolframe_types::MediaId;

use super::TokenScope;
use crate::SessionId;

/// `RedeemPlayToken`: a play token, redeemed on the server that minted it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedeemPlayToken {
    pub token: String,
}

impl fmt::Debug for RedeemPlayToken {
    // The token opens an item until it is used: keep it out of logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RedeemPlayToken { token: <redacted> }")
    }
}

/// What a redeemed play token opened (`Reply::PlayTokenRedeemed`). The
/// session's `Debug` keeps it out of logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayTokenRedeemed {
    /// The session to name in `StartPlayback` and the playback's
    /// controls: bound to the endpoint that redeemed it, and kept while
    /// it is used. It reaches nothing else.
    pub session: SessionId,
    /// The item it opens, by this server's own id.
    pub item: MediaId,
    pub scope: TokenScope,
}

/// Where a paired server is, as its pair keeps it (`RemoteHints`): what a
/// device dials it at. SB's own record of SA, from the pairing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteHints {
    /// SA's [`crate::ServerId`].
    pub server_id: String,
    /// SA's endpoint id, hex: the key the device pins for the dial.
    pub endpoint_id: String,
    /// `ip:port` addresses SA was paired at.
    #[serde(default)]
    pub addrs: Vec<String>,
    /// Relay URLs SA named.
    #[serde(default)]
    pub relays: Vec<String>,
    /// Whether SB holds a connection to SA now. Offline, a grant is
    /// refused at once (`OriginOffline`).
    pub online: bool,
}

/// SA's token for the device (`RemoteGrant`): redeem it on SA's
/// `client/1` (`RedeemPlayToken`) from the device that asked.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteGranted {
    pub token: String,
    /// The item, by SA's own id: what to name in `StartPlayback` on SA.
    pub item: MediaId,
    /// SA's endpoint id, hex.
    pub endpoint_id: String,
    /// When the token stops working, Unix seconds: minutes from now.
    pub expires_at: i64,
    /// For a download token (P8-07, `RemoteGrant` with a `download`): the
    /// download's id on SA, what the device reads there
    /// (`ResourceRequest::Download`) with the session the token opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download: Option<String>,
}

impl fmt::Debug for RemoteGranted {
    // The token opens an item until it is used: keep it out of logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteGranted")
            .field("token", &"<redacted>")
            .field("item", &self.item)
            .field("endpoint_id", &self.endpoint_id)
            .field("expires_at", &self.expires_at)
            .field("download", &self.download)
            .finish()
    }
}
