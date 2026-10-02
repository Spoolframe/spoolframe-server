//! The `spoolframe/client/1` wire protocol: messages, framing and ALPN
//! names.
//!
//! Permissively licensed (MIT OR Apache-2.0); it is the boundary that
//! proprietary clients talk across, so it never depends on an AGPL crate.

/// The ALPN of the client protocol, version 1. A breaking change to the
/// protocol is a new ALPN, so old and new clients can be served side by
/// side.
pub const CLIENT_ALPN: &[u8] = b"spoolframe/client/1";

/// The ALPN of server-to-server federation, version 1 (ARCHITECTURE §26).
/// Reserved: a server refuses it with [`close::UNSUPPORTED_PROTOCOL`] until
/// federation exists.
pub const FEDERATION_ALPN: &[u8] = b"spoolframe/federation/1";

/// QUIC application close codes a Spoolframe peer sends, with a
/// human-readable reason beside them.
pub mod close {
    /// The ALPN is known but this peer does not serve it.
    pub const UNSUPPORTED_PROTOCOL: u32 = 1;
}
