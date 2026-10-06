//! The `spoolframe/client/1` wire protocol: messages, framing and ALPN
//! names.
//!
//! Permissively licensed (MIT OR Apache-2.0); it is the boundary that
//! proprietary clients talk across, so it never depends on an AGPL crate.
//!
//! One connection carries many QUIC streams ([`streams`]). Every message
//! is a length-prefixed CBOR frame ([`codec`]); the messages are in
//! [`messages`].

pub mod artwork;
pub mod codec;
pub mod device_sign_in;
pub mod fingerprint;
pub mod lan;
pub mod media;
pub mod messages;
pub mod passkeys;
pub mod playback;
pub mod plex;
pub mod policy;
pub mod previews;
pub mod resource;
pub mod rpc;
pub mod search;
pub mod streams;

pub use artwork::{ArtworkReply, ArtworkRequest, ArtworkResponse, ArtworkSize};
pub use media::{MAX_READ, MediaRead, MediaReadReply, MediaReadResponse, PlaybackHandle};
pub use messages::{
    AuthMethod, ConfirmPassword, DeviceInfo, DeviceSignInRequest, Error, Event, Hello, HomeScreen,
    LoginPassword, MAX_HELLO_TEXT, ManifestKind, PROTOCOL_VERSION, PinProof, PinState, Reply,
    Request, RequestEnvelope, Response, SUBTITLE_CHUNK, ServerInfo, SessionId, SessionInfo,
    UserSummary,
};
pub use passkeys::{
    APP_ORIGIN, APP_RP_ID, FinishPasskeyRegistration, FinishPasskeySignIn, NONCE_LEN,
    PasskeyCreation, PasskeyInfo, PasskeyRegistrationBegun, PasskeyRequest, PasskeySignInBegun,
    app_passkey_challenge,
};
pub use playback::{
    AudioAction, BitmapSubtitles, ChosenTracks, ConvertedAudio, ConvertedSubtitle, ConvertedVideo,
    PlannedAudio, PlannedSubtitle, PlaybackPlan, Selection, SubtitleAction, SubtitleDelivery,
    TrackReason,
};
pub use plex::{PlexLink, PlexSyncSummary};
pub use previews::{PreviewTile, Previews};
pub use search::{SearchCounts, SearchGroup, SearchRequest, SearchResults, TopResult};

/// Development ALPN. Breaking revisions currently retain `/1` and require
/// a coordinated server/SDK update; [`PROTOCOL_VERSION`] identifies the revision.
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
    /// The server holds as many connections from devices with no session
    /// as it takes (PX-10). A device that is signed in is always let in;
    /// one that is not tries again later.
    pub const BUSY: u32 = 2;
    /// The device's last live session on this server ended (signed out,
    /// revoked, or its user disabled). It may connect again, as a device
    /// that is not signed in.
    pub const SIGNED_OUT: u32 = 3;
}
