//! The types every Spoolframe client sees: the media model and its
//! identifiers.
//!
//! These are Spoolframe's models, never Plex's (ARCHITECTURE §5). Plex is
//! only today's source of metadata; when a native catalog replaces it, these
//! types stay as they are.
//!
//! Permissively licensed (MIT OR Apache-2.0) so that any client, open or
//! proprietary, can link it. It never depends on an AGPL server crate.

pub mod codec;
mod duration_ms;
pub mod id;
pub mod media;
pub mod model;

pub use codec::{AudioCodec, ClientCapabilities, Container, SubtitleFormat, VideoCodec};
pub use id::{ArtworkId, IdError, LibraryId, MediaId, MediaKind, Namespace};
pub use media::{AudioTrack, MediaTracks, SubtitleTrack};
pub use model::{
    Album, Artist, Artwork, ArtworkKind, Episode, Item, Library, LibraryKind, Movie, PlaybackState,
    Season, Show, Track,
};
