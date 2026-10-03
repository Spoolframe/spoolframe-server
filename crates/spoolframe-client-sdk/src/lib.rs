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

mod client;
mod connection;
mod discovery;
mod error;
mod link;
mod servers;
mod session;
pub mod storage;

pub use client::{Client, Device, DeviceSignIn, ServersWatch};
pub use error::SdkError;
pub use link::{ConnectionState, StateWatch};
pub use servers::ServerEntry;
pub use session::{
    Chunk, ItemDetail, ItemsPage, Plan, Playback, PlaybackChange, PlaybackOptions, Session,
    Subtitle,
};
// The wire types the calls take and return, so an app needs only the SDK.
pub use spoolframe_protocol::{
    ArtworkSize, BitmapSubtitles, ChosenTracks, DeviceInfo, DeviceSignInRequest, HomeScreen,
    MAX_READ, ManifestKind, PlaybackHandle, PlaybackPlan, Selection, SessionInfo, TrackReason,
    UserSummary,
};
// Fingerprints for the "Server identity changed" alert (P1-13, P1-21).
pub use spoolframe_protocol::fingerprint;
// Sign-in codes and the links their QR codes carry (P3-07).
pub use spoolframe_protocol::device_sign_in;
pub use spoolframe_types as types;
pub use storage::{MemoryStorage, Storage, StorageError};
