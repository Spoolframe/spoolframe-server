//! The SDK for Spoolframe client apps: it finds and dials servers over
//! Iroh, keeps the saved servers, signs in, and plays media.
//!
//! Start with [`Client::open`], then open a [`Session`] on a saved server.
//! A Rust app uses the SDK directly; an app in another language wraps it
//! through FFI.
//!
//! Licensed MIT OR Apache-2.0, so proprietary apps can link it. It never
//! depends on an AGPL crate.
//!
//! The public surface is concrete types with async methods and owned
//! values, no generics, so a UniFFI layer can wrap it. What the SDK
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
mod downloads;
mod error;
mod experimental;
mod federation;
mod invite_admin;
mod invites;
mod lan_servers;
mod link;
mod link_meter;
pub mod pairing;
mod pairing_relay;
mod playlists;
mod plays;
pub mod portable;
mod portable_sign_in;
mod reachability;
mod refusals;
mod relay_policy;
mod servers;
mod session;
pub mod storage;
mod subtitle_search;
mod subtitle_sync;

pub use client::{Client, ClientOptions, Device, DeviceSignIn, STREAM_WINDOW, ServersWatch};
pub use connection::Timeouts;
// Downloads for offline viewing, and their wire types.
pub use downloads::{
    DownloadEvent, DownloadPhase, DownloadSpace, Downloads, LocalArtwork, LocalDownload,
    LocalWatch, SyncedWatch,
};
// Music offline, and its wire types.
pub use downloads::{
    AlbumTrack, CacheSpace, DEFAULT_CACHE_MAX_BYTES, DownloadSettings, MusicDownload, MusicQuality,
    MusicSynced, MusicTarget, NetworkKind, OfflineAlbum, OfflineArtist, OfflineMark, OfflineMusic,
    OfflinePlaylist, OfflineTrack, SyncedPlay,
};
pub use error::SdkError;
pub use experimental::{Experimental, HttpPlayback, HttpPlaybackRefusal};
pub use invite_admin::{
    InviteCreator, InviteKind, InviteOrigin, InviteSpec, InviteSummary, MintedInvite,
};
pub use invites::{InviteInfo, InviteLookup, InviteOutcome, parse_code};
pub use lan_servers::LanServerInfo;
pub use link::{ConnectionPath, ConnectionState, SignOutWatch, StateWatch};
pub use link_meter::LinkMeter;
pub use spoolframe_protocol::downloads::{
    DownloadFailure, DownloadFile, DownloadOptionsInfo, DownloadSource, DownloadState,
    DownloadTracks, FileTracks, LeftOutSubtitle, OfflineWatchOutcome, OriginalDownload,
};
pub use spoolframe_protocol::federation::{PairingRefused, PeerServer};
// Pairing a guest onto this device by QR code.
pub use pairing::{Connected, Paired, Pairing, PairingError, PairingOffer};
pub use spoolframe_protocol::invite::InviteLinkKind;
pub use spoolframe_protocol::pairing::PairedServer;
// Portable identities and their device credentials.
pub use federation::{DIRECT_DIAL, LibrariesChanged, RemoteDownload, RemoteMode, RemotePlayback};
pub use pairing_relay::{RelayAnswer, RelayOffer, RelayPairing};
pub use playlists::{PlaylistChange, PlaylistEvent, PlaylistList, PlaylistUpdate};
pub use portable::{
    CredentialError, DevicePublicKey, GuestPairing, GuestServer, HANDLE_LEN, IdentityGenesis,
    IdentityRecord, KeyAssertion, MAX_IDENTITY_NAME, PORTABLE_RP, PhraseError, Portable,
    PortableIdentity, PortableIdentityState, PortableRoot, PortableUserId, RecoveryPhrase,
    RevokedKey, RootKey, RootLocation, RootRegistration, RootSet, RootSignature, RootSigner,
    RootSyncRefused, SecurityKey, SecurityKeyError, SoftSecurityKey, StateError, TieProof,
    UserDeviceCredential, WebAuthnError,
};
pub use reachability::{
    DialReport, DialStage, KnownPath, LivePath, LookupOutcome, LookupResult, PathKind,
    Reachability, SavedAddr,
};
pub use spoolframe_protocol::federation::{RemoteGranted, RemoteHints};
pub use subtitle_sync::SubtitleTimingEvent;
// Playlists' wire types, as the calls take and return them.
pub use servers::ServerEntry;
pub use session::{
    ActiveProfile, Chunk, ChunkStream, ItemDetail, ItemsPage, Plan, Playback, PlaybackChange,
    PlaybackOptions, PlaybackQuality, ProfileSignIn, SavedProfile, Session, SignInMethod, Subtitle,
};
pub use spoolframe_protocol::playlists::{
    Builtin, EntryId, ItemKind, Kinds, Liked, PinNote, PinNoteReason, PlaySettings, PlaylistEdit,
    PlaylistEntry, PlaylistNext, PlaylistOwnerKind, PlaylistPage, PlaylistRef, PlaylistShare,
    PlaylistSummary, PlexImport, PlexImported, PlexUnmapped, Repeat, ShareCandidate,
    ShareCandidatePage, UserKind, Visibility,
};
// The wire types the calls take and return, so an app needs only the SDK.
pub use spoolframe_protocol::{
    AddressNetwork, ArtworkSize, BitmapSubtitles, ChosenTracks, DeviceAddress, DeviceInfo,
    DeviceSignInRequest, HomeScreen, MAX_READ, ManifestKind, PinProof, PinState, PlaybackHandle,
    PlaybackPlan, PlexLink, PlexSyncSummary, PortableTie, PreviewTile, Previews, SearchCounts,
    SearchGroup, SearchRequest, SearchResults, Selection, SessionInfo, TopResult,
    TrackPreferencesInfo, TrackReason, TrustedProfile, UserSummary,
};
// Experimental federation: a search's hits and a title's copies.
pub use spoolframe_protocol::{Copies, SearchHit, TitleCopy};
// Passkeys in the app: the raw WebAuthn bytes the platform's
// passkey API takes and gives. The relying party is each server's own,
// named in the request. `LEGACY_APP_RP_ID` marks passkeys made for a
// shared relying party; they sign in nowhere.
pub use spoolframe_protocol::{
    FinishPasskeyRegistration, FinishPasskeySignIn, LEGACY_APP_RP_ID, PasskeyCreation, PasskeyInfo,
    PasskeyRequest, ServerId,
};
// Fingerprints for the "Server identity changed" alert.
pub use spoolframe_protocol::fingerprint;
// Sign-in codes and the links their QR codes carry.
pub use spoolframe_protocol::device_sign_in;
// Invite links: the codec, for an app that makes or checks one.
pub use spoolframe_protocol::invite;
pub use spoolframe_types as types;
pub use storage::{MemoryStorage, Storage, StorageError};

mod managed;
pub use managed::{ManagedPlayback, PlaybackBackend, PlaybackShare, SegmentStream};
mod profile_scope;
pub use profile_scope::{ProfileScope, ProfileToken};

#[cfg(feature = "media-readers")]
pub mod progressive;
#[cfg(feature = "media-readers")]
pub mod reader;
#[cfg(all(test, feature = "media-readers"))]
mod reader_tests;
