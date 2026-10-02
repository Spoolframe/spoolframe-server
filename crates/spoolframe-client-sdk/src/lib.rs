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

pub use client::{Client, Device, ServersWatch};
pub use error::SdkError;
pub use link::{ConnectionState, StateWatch};
pub use servers::ServerEntry;
pub use session::{ItemDetail, ItemsPage, Plan, Playback, Session, Subtitle};
// The wire types the calls take and return, so an app needs only the SDK.
pub use spoolframe_protocol::{
    ArtworkSize, ChosenTracks, HomeScreen, MAX_READ, PlaybackHandle, PlaybackPlan, Selection,
    SessionInfo, TrackReason, UserSummary,
};
pub use spoolframe_types as types;
pub use storage::{MemoryStorage, Storage, StorageError};
