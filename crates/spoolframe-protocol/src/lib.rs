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
pub mod downloads;
pub mod experimental;
pub mod features;
pub mod federation;
pub mod fingerprint;
pub mod invite;
pub mod invite_admin;
pub mod lan;
pub mod media;
pub mod messages;
pub mod pairing;
pub mod passkeys;
pub mod playback;
pub mod playlists;
pub mod plex;
pub mod policy;
pub mod portable;
pub mod previews;
pub mod relay_access;
pub mod resource;
pub mod rpc;
pub mod search;
pub mod server_id;
pub mod streams;

pub use artwork::{ArtworkReply, ArtworkRequest, ArtworkResponse, ArtworkSize};
pub use media::{MAX_READ, MediaRead, MediaReadReply, MediaReadResponse, PlaybackHandle};
pub use messages::{
    AddressNetwork, AuthMethod, ChangePassword, ConfirmPassword, DeviceAddress, DeviceInfo,
    DeviceSignInRequest, Error, Event, Hello, HomeScreen, InviteLookupOutcome, LoginPassword,
    LookupInvite, MAX_HELLO_TEXT, ManifestKind, PROTOCOL_VERSION, PinProof, PinState,
    PortableSignIn, PortableTie, RedeemInvite, RedeemPortableInvite, Reply, Request,
    RequestEnvelope, Response, SUBTITLE_CHUNK, ServerInfo, SessionId, SessionInfo,
    TrackPreferencesInfo, TrustedProfile, UserSummary, WhoAmIReply,
};
pub use passkeys::{
    FinishPasskeyRegistration, FinishPasskeySignIn, LEGACY_APP_RP_ID, NONCE_LEN, PasskeyCreation,
    PasskeyInfo, PasskeyRegistrationBegun, PasskeyRequest, PasskeySignInBegun,
    app_passkey_challenge, is_app_rp_id,
};
pub use playback::{
    AudioAction, BitmapSubtitles, ChosenTracks, ConvertedAudio, ConvertedSubtitle, ConvertedVideo,
    PlannedAudio, PlannedSubtitle, PlaybackPlan, Quality, Rung, Selection, SubtitleAction,
    SubtitleDelivery, TrackReason,
};
pub use plex::{PlexLink, PlexSyncSummary};
pub use portable::{
    CredentialError, DevicePublicKey, InvalidDevicePublicKey, InvalidPortableUserId,
    PORTABLE_USER_ID_LEN, PortableIdentityState, PortableUserId, RevokedKey, StateError, TieProof,
    TieProofError, UserDeviceCredential, tie_message,
};
pub use previews::{PreviewTile, Previews};
pub use search::{SearchCounts, SearchGroup, SearchRequest, SearchResults, TopResult};
pub use server_id::{APP_RP_DOMAIN, InvalidServerId, SERVER_ID_LEN, ServerId};

/// Development ALPN. Breaking revisions currently retain `/1` and require
/// a coordinated server/SDK update; [`PROTOCOL_VERSION`] identifies the revision.
pub const CLIENT_ALPN: &[u8] = b"spoolframe/client/1";

/// The ALPN of server-to-server federation, version 1 (ARCHITECTURE §26):
/// [`federation`]. Experimental (P8-01): a server whose
/// `experimental.federation` is off refuses it with
/// [`close::UNSUPPORTED_PROTOCOL`].
pub const FEDERATION_ALPN: &[u8] = b"spoolframe/federation/1";

/// The ALPN a device pairs with a portable identity's phone on (P7-03),
/// directly, with no server: [`pairing`].
pub use pairing::PAIR_ALPN;

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
    /// Federation (P8-01): the dialling server's endpoint is paired under
    /// another server id. Refused every time.
    pub const SERVER_ID_CHANGED: u32 = 4;
    /// Federation (P8-01): the two servers no longer pair; the one closing
    /// revoked its side.
    pub const UNPAIRED: u32 = 5;
}
