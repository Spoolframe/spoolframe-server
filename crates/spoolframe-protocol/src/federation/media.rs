//! The media path: what the client protocol carries for playing a
//! paired server's item.
//!
//! The device, not its home server, decides how it plays an item of a
//! paired server (the item's server). It asks the home server where the
//! item's server is (`RemoteHints`) and dials it with a short budget. If it
//! gets through, it asks the home server for a token for itself
//! (`RemoteGrant`). If not, it plays the item from the home server
//! (`StartPlayback` of the `fed:` id), which plays from the item's server
//! in its own name and on to the device.
//!
//! The item's server mints the token for one of the home server's users
//! ([`super::TokenGranted`]). Whoever plays redeems it on the item's
//! server's own `spoolframe/client/1` (`Request::RedeemPlayToken`): the
//! device itself when it can reach that server (direct), or the home
//! server, which plays on to the device (proxy). Either way the item's
//! server answers with a session that opens that one item's playback and
//! its controls, and nothing else; `spoolframe/federation/1` carries no
//! media.
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
/// device dials it at. The home server's own record of the item's server,
/// from the pairing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteHints {
    /// The item's server's [`crate::ServerId`].
    pub server_id: String,
    /// The item's server's endpoint id, hex: the key the device pins for
    /// the dial.
    pub endpoint_id: String,
    /// `ip:port` addresses the item's server was paired at.
    #[serde(default)]
    pub addrs: Vec<String>,
    /// Relay URLs the item's server named.
    #[serde(default)]
    pub relays: Vec<String>,
    /// Whether the home server holds a connection to the item's server
    /// now. Offline, a grant is
    /// refused at once (`OriginOffline`).
    pub online: bool,
}

/// The item's server's token for the device (`RemoteGrant`): redeem it on
/// that server's `client/1` (`RedeemPlayToken`) from the device that
/// asked.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteGranted {
    pub token: String,
    /// The item, by the item's server's own id: what to name in
    /// `StartPlayback` on that server.
    pub item: MediaId,
    /// The item's server's endpoint id, hex.
    pub endpoint_id: String,
    /// When the token stops working, Unix seconds: minutes from now.
    pub expires_at: i64,
    /// For a download token (`RemoteGrant` with a `download`): the
    /// download's id on the item's server, what the device reads there
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
