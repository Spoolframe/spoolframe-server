//! The client side of Spoolframe: the Iroh transport, saved servers and
//! sessions. The Linux client uses it directly, the tvOS client through FFI.
//!
//! Permissively licensed (MIT OR Apache-2.0) so proprietary clients can
//! link it; it never depends on an AGPL crate.
//!
//! The public surface is concrete types with async methods and owned
//! values, no generics, so a UniFFI layer can wrap it (P5). What the SDK
//! needs from the platform comes in from the app: the [`Device`] and its
//! [`Storage`].
//!
//! Every request has a time limit ([`Timeouts`]); one not answered in time
//! is [`SdkError::TimedOut`]. An app calls [`Client::resume`] when it comes
//! back to the foreground or the device wakes, so connections that died
//! while it was away are dropped before a request waits on one.

mod client;
mod connection;
mod discovery;
mod error;
mod link;
mod servers;
mod session;
pub mod storage;

pub use client::{Client, ClientOptions, Device, DeviceSignIn, ServersWatch};
pub use connection::Timeouts;
pub use error::SdkError;
pub use link::{ConnectionPath, ConnectionState, SignOutWatch, StateWatch};
pub use servers::ServerEntry;
pub use session::{
    ActiveProfile, Chunk, ItemDetail, ItemsPage, Plan, Playback, PlaybackChange, PlaybackOptions,
    ProfileSignIn, SavedProfile, Session, SignInMethod, Subtitle,
};
// The wire types the calls take and return, so an app needs only the SDK.
pub use spoolframe_protocol::{
    ArtworkSize, BitmapSubtitles, ChosenTracks, DeviceInfo, DeviceSignInRequest, HomeScreen,
    MAX_READ, ManifestKind, PinProof, PinState, PlaybackHandle, PlaybackPlan, PlexLink,
    PlexSyncSummary, PreviewTile, Previews, SearchCounts, SearchGroup, SearchRequest,
    SearchResults, Selection, SessionInfo, TopResult, TrackPreferencesInfo, TrackReason,
    TrustedProfile, UserSummary,
};
// Passkeys in the app (P3-19): the raw WebAuthn bytes the platform's
// passkey API takes and gives. The relying party is each server's own
// (PX-14), in the request; `LEGACY_APP_RP_ID` marks passkeys made for the
// one all servers once shared, which sign in nowhere now.
pub use spoolframe_protocol::{
    FinishPasskeyRegistration, FinishPasskeySignIn, LEGACY_APP_RP_ID, PasskeyCreation, PasskeyInfo,
    PasskeyRequest, ServerId,
};
// Fingerprints for the "Server identity changed" alert (P1-13, P1-21).
pub use spoolframe_protocol::fingerprint;
// Sign-in codes and the links their QR codes carry (P3-07).
pub use spoolframe_protocol::device_sign_in;
pub use spoolframe_types as types;
pub use storage::{MemoryStorage, Storage, StorageError};

mod managed;
pub use managed::{ManagedPlayback, PlaybackBackend};
mod profile_scope;
pub use profile_scope::{ProfileScope, ProfileToken};

#[cfg(feature = "media-readers")]
pub mod progressive;
#[cfg(feature = "media-readers")]
pub mod reader;
#[cfg(all(test, feature = "media-readers"))]
mod reader_tests;
