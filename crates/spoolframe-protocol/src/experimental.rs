//! Experimental features (PX-49): messages a client and server use only
//! when both said so in `Hello`.
//!
//! A client names the features it wants in [`Hello::experimental`]; the
//! server's answer, [`ServerInfo::experimental`], names those it turned on
//! for the connection: the ones it knows and has on. Neither side assumes
//! a feature the other did not name, and a name either does not know is
//! ignored, never an error. An older server ignores the field, and its
//! answer names none.
//!
//! An experimental feature's messages are outside the protocol's
//! stability: they may change or go away without [`PROTOCOL_VERSION`]
//! moving, and PX-16's version 1 does not promise them. One graduates by
//! becoming an ordinary message of a protocol version, and its name goes.
//!
//! [`Hello::experimental`]: crate::Hello::experimental
//! [`ServerInfo::experimental`]: crate::ServerInfo::experimental
//! [`PROTOCOL_VERSION`]: crate::PROTOCOL_VERSION

use serde::{Deserialize, Serialize};

/// A playback's HLS over plain HTTP, from the server's own port, for a
/// player with no Iroh (AirPlay first): `Request::HttpPlaybackUrl`.
pub const HTTP_PLAYBACK: &str = "http-playback";

/// The most feature names a server keeps of a `Hello`; the rest are
/// dropped, as each name is cut to `MAX_HELLO_TEXT` characters (PX-10).
pub const MAX_ASKED: usize = 16;

/// The features of `asked` that are among `on`, each once, in `asked`'s
/// order: what a server's answer names. A name not among `on` is ignored.
pub fn turned_on(asked: &[String], on: &[&str]) -> Vec<String> {
    let mut turned: Vec<String> = Vec::new();
    for name in asked {
        if on.contains(&name.as_str()) && !turned.contains(name) {
            turned.push(name.clone());
        }
    }
    turned
}

/// Why the server gives no HTTP URL for a playback (`http-playback`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpPlaybackRefusal {
    /// The feature is not on for this connection: the server's HTTP
    /// playback port is off, or the client's `Hello` did not name it.
    Off,
    /// The client reached the server through a relay, or over the
    /// Internet, and the server names no address for that
    /// (`http_playback.address`): there is no address on the client's
    /// network to give.
    RelayOnly,
    /// No playback of this session's has that handle: it ended, or is
    /// another's.
    NoSuchPlayback,
    /// The playback is planned as something the HTTP side does not serve:
    /// a conversion read as one growing file rather than HLS.
    Unsupported,
    /// A reason this build does not know.
    #[serde(other)]
    Unknown,
}
