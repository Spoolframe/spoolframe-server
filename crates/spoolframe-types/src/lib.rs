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
pub mod codec_string;
pub mod date;
mod duration_ms;
pub mod id;
pub mod language;
pub mod media;
pub mod model;
pub mod platform;
pub mod preferences;
pub mod required;
pub mod role;
pub mod search;
#[cfg(test)]
mod test_support;

pub use codec::{
    AudioCodec, ClientCapabilities, Container, Delivery, DisplayCapabilities, HdrFormat,
    SubtitleFormat, VideoCodec, VideoLimits,
};
pub use codec_string::{CodecString, CodecStringError, Tier};
pub use date::{CalendarDate, DateError};
pub use id::{ArtworkId, IdError, LibraryId, MediaId, MediaKind, Namespace, PersonId};
pub use media::{
    AudioRole, AudioTrack, MediaTracks, QuotaOf, SubtitleDownload, SubtitleDownloaded,
    SubtitleOrigin, SubtitleResult, SubtitleRole, SubtitleSearch, SubtitleSource,
    SubtitleSourceState, SubtitleSourceStatus, SubtitleSync, SubtitleTiming, SubtitleTrack,
    VideoTrack,
};
pub use model::{
    Album, Artist, Artwork, ArtworkKind, CastMember, Episode, Item, Library, LibraryKind,
    LibraryServer, Marker, MarkerKind, Movie, PlaybackState, Season, Show, Track,
};
pub use platform::Platform;
pub use preferences::{
    AudioPreferences, BitmapPreference, ForcedPreference, SdhPreference, SubtitleMode,
    SubtitlePreferences, TrackPreferences,
};
pub use role::Role;
pub use search::{Person, PersonRole, SearchKind, SearchScope};

pub mod pin;

/// The domain every server's app passkeys are made under: a server's
/// relying party is `<server id>.spoolframe.app` (PX-14). Kept here, the
/// crate every client and the admin API share, so all of them name the
/// same one.
pub const APP_RP_DOMAIN: &str = "spoolframe.app";
