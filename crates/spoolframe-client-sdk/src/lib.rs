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

mod app_links;
mod catalog_cache;
mod catalog_view;
mod client;
mod connection;
pub mod describe;
mod discovery;
mod downloads;
mod error;
mod experimental;
mod federation;
pub mod gate;
mod home_network;
mod invite_admin;
mod invites;
mod lan_servers;
mod likes;
mod link;
mod link_meter;
mod paired;
pub mod pairing;
mod pairing_relay;
mod playlist_cursor;
mod playlist_edits;
pub mod playlists;
mod plays;
pub mod portable;
mod portable_sign_in;
mod profile_events;
mod profile_store;
mod quality;
pub mod queue;
mod reachability;
mod refusals;
mod relay_policy;
mod servers;
mod session;
mod sign_in_rules;
pub mod storage;
mod subtitle_search;
mod subtitle_sync;
mod watch_events;
pub mod webauthn;
mod whos_watching;

pub use app_links::{AppLink, invite_share_link, is_invite_link, parse_app_link};
pub use catalog_cache::{CacheConfig, CatalogChanged, CatalogChanges, CatalogKey};
pub use catalog_view::{
    AlbumTracks, ItemsSource, LibraryPager, SeasonEpisodes, ShowDetail, ShowTree, WatchScope,
};
pub use client::{Client, ClientOptions, Device, DeviceSignIn, STREAM_WINDOW, ServersWatch};
pub use connection::Timeouts;
pub use profile_store::{ProfileStore, profile_hash};
pub use sign_in_rules::{
    AddressClass, PASSWORD_MIN, PIN_MAX_DIGITS, PIN_MIN_DIGITS, PasswordProblem, Pin, PinFailure,
    PinRefusal, address_class, password_problem,
};
pub use whos_watching::{
    LastProfile, LaunchTarget, PairedSignIn, ProfileOpens, WatchingProfile, WhosWatching,
};
// Downloads for offline viewing, and their wire types.
pub use downloads::{
    Applied, BatchStart, ChildPlan, DownloadAction, DownloadLibrary, DownloadList, DownloadedShow,
    DownloadsHere, FailedChild, ProfileHere, REFRESH_EVERY, RefreshMissed, RefreshReport,
    RunProgress, RunTally, SkipReason, SkippedChild, UNITS_PER_DOWNLOAD,
};
pub use downloads::{
    DownloadEvent, DownloadPhase, DownloadSpace, Downloads, LocalArtwork, LocalDownload,
    LocalWatch, RunCancel, RunEnd, SyncedWatch, WATCHED_NEAR_END,
};
// Music offline, and its wire types.
pub use downloads::{
    AlbumTrack, CacheSpace, DEFAULT_CACHE_MAX_BYTES, DownloadSettings, MusicDownload, MusicPhase,
    MusicQuality, MusicSynced, MusicTarget, NetworkKind, OfflineAlbum, OfflineArtist, OfflineMark,
    OfflineMusic, OfflinePlaylist, OfflineTrack, SyncedPlay,
};
pub use error::{ErrorKind, SdkError};
pub use experimental::{Experimental, HttpPlayback, HttpPlaybackRefusal};
pub use gate::{Gate, Invited, On};
pub use invite_admin::{
    InviteCreator, InviteKind, InviteOrigin, InviteSpec, InviteSummary, MintedInvite,
};
pub use invites::{
    InviteInfo, InviteLookup, InviteNext, InviteOutcome, InviteRefusal, invite_next, parse_code,
};
pub use lan_servers::LanServerInfo;
pub use link::{ConnectionPath, ConnectionState, SignOutWatch, StateWatch};
pub use link_meter::LinkMeter;
pub use spoolframe_protocol::downloads::{
    DownloadFailure, DownloadFile, DownloadOptionsInfo, DownloadSource, DownloadState,
    DownloadTracks, FailureRemedy, FileTracks, LeftOutSubtitle, OfflineWatchOutcome,
    OriginalDownload,
};
pub use spoolframe_protocol::federation::{PairingRefused, PeerServer};
pub use spoolframe_protocol::{Feature, Features, NextMode};
// Pairing a guest onto this device by QR code.
pub use pairing::{Connected, Paired, Pairing, PairingError, PairingOffer};
pub use spoolframe_protocol::invite::InviteLinkKind;
pub use spoolframe_protocol::pairing::PairedServer;
// Portable identities and their device credentials.
pub use federation::{DIRECT_DIAL, LibrariesChanged, RemoteDownload, RemoteMode, RemotePlayback};
pub use likes::{LikeCache, Pending};
pub use paired::{LibraryDiff, LibraryGroup, PairedServers, ServerOf, SharingServer, Unshared};
pub use pairing_relay::{RelayAnswer, RelayOffer, RelayPairing};
pub use playlist_cursor::{PlayFrom, PlaylistCursor, PlaylistSource, UpNext, UpNextNote};
pub use playlist_edits::{Edited, PLAYLIST_PAGE};
pub use playlists::{PlaylistChange, PlaylistEdited, PlaylistEvent, PlaylistList, PlaylistUpdate};
pub use portable::{
    CredentialError, DevicePublicKey, GuestPairing, GuestServer, HANDLE_LEN, IdentityGenesis,
    IdentityRecord, KeyAssertion, MAX_IDENTITY_NAME, PORTABLE_RP, PhraseError, Portable,
    PortableIdentity, PortableIdentityState, PortableRoot, PortableUserId, RecoveryPhrase,
    RevokedKey, RootKey, RootLocation, RootRegistration, RootSet, RootSignature, RootSigner,
    RootSyncRefused, SecurityKey, SecurityKeyError, SoftSecurityKey, StateError, TieProof,
    UserDeviceCredential, WebAuthnError,
};
pub use profile_events::{ProfileChange, ProfileEvent, SessionEndReason};
pub use quality::NotAQuality;
pub use reachability::{
    DialReport, DialStage, KnownPath, LivePath, LookupOutcome, LookupResult, PathKind,
    Reachability, SavedAddr, iso_utc,
};
pub use spoolframe_protocol::federation::{RemoteGranted, RemoteHints};
pub use subtitle_sync::SubtitleTimingEvent;
pub use watch_events::WatchEvent;
// Playlists' wire types, as the calls take and return them.
pub use servers::{ServerEntry, display_host};
pub use session::{
    ActiveProfile, Chunk, ChunkStream, ItemDetail, ItemsPage, Plan, Playback, PlaybackChange,
    PlaybackOptions, PlaybackQuality, ProfileSignIn, Reauth, SavedProfile, Session, SignInMethod,
    Subtitle,
};
pub use spoolframe_protocol::playlists::{
    Builtin, EntryId, ItemKind, Kinds, Liked, PinNote, PinNoteReason, PlaySettings, PlaylistEdit,
    PlaylistEntry, PlaylistNext, PlaylistOwnerKind, PlaylistPage, PlaylistRef, PlaylistShare,
    PlaylistSummary, PlexImport, PlexImported, PlexUnmapped, Repeat, ShareCandidate,
    ShareCandidatePage, UserKind, Visibility,
};
// The wire types the calls take and return, so an app needs only the SDK.
pub use spoolframe_protocol::{
    AddressNetwork, ArtworkSize, AudioAction, BitmapSubtitles, ChosenTracks, ConvertedAudio,
    ConvertedSubtitle, ConvertedVideo, DeviceAddress, DeviceInfo, DeviceSignInRequest, HomeScreen,
    MAX_READ, ManifestKind, PinProof, PinState, PlannedAudio, PlannedSubtitle, PlaybackHandle,
    PlaybackPlan, PlexLink, PlexSyncSummary, PortableTie, PreviewTile, Previews, Quality, Rung,
    SearchCounts, SearchGroup, SearchRequest, SearchResults, Selection, SessionInfo,
    SubtitleAction, SubtitleDelivery, TopResult, TrackPreferencesInfo, TrackReason, TrustedProfile,
    UserSummary,
};
// A plan as an info panel shows it ([`PlaybackPlan::summary`]).
pub use spoolframe_protocol::{
    AudioSummary, PlanKind, PlanSummary, SubtitleHow, SubtitleSummary, VideoSummary,
};
// Experimental federation: a search's hits and a title's copies.
pub use spoolframe_protocol::{Copies, HiddenUnder, PlayFromChoice, SearchHit, TitleCopy};
// What a TV features from Home ([`HomeScreen::featured`]).
pub use spoolframe_protocol::{Featured, FeaturedLimits, FeaturedRow};
// Passkeys in the app: the raw WebAuthn bytes the platform's
// passkey API takes and gives. The relying party is each server's own,
// named in the request. `LEGACY_APP_RP_ID` marks passkeys made for a
// shared relying party; they sign in nowhere.
pub use spoolframe_protocol::{
    FinishPasskeyRegistration, FinishPasskeySignIn, LEGACY_APP_RP_ID, MAX_NAME_CHARS,
    PasskeyCreation, PasskeyInfo, PasskeyRequest, ServerId, fit_name,
};
// Fingerprints for the "Server identity changed" alert.
pub use spoolframe_protocol::fingerprint;
// Sign-in codes and the links their QR codes carry.
pub use spoolframe_protocol::device_sign_in;
// Invite links: the codec, for an app that makes or checks one.
pub use spoolframe_protocol::invite;
pub use spoolframe_types as types;
pub use storage::{MemoryStorage, Storage, StorageError};

#[cfg(feature = "uniffi")]
mod uniffi_types;
#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

mod managed;
mod markers;
pub use markers::{
    MarkerAction, MarkerOffer, MarkerTracker, Markers, SKIP_END_MARGIN, UNDO_WINDOW, UP_NEXT_LEAD,
    UP_NEXT_MIN_LENGTH, credits_start,
};
pub mod tracks;
pub use managed::{
    ManagedPlayback, PlaybackBackend, PlaybackEvent, PlaybackShare, PlaybackStopped, PlayerSession,
    PlayerState, RestartCause, SegmentStream, SubtitleFile,
};
pub mod driver;
pub use driver::{LocalSpawn, PlaybackController, PlaybackDriver};
mod profile_scope;
pub use profile_scope::{ProfileScope, ProfileToken};

#[cfg(feature = "media-readers")]
pub mod progressive;
#[cfg(feature = "media-readers")]
pub mod reader;
#[cfg(all(test, feature = "media-readers"))]
mod reader_tests;
