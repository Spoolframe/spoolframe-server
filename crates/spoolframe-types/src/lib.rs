//! The types every Spoolframe client sees: the media model and its
//! identifiers.
//!
//! These are Spoolframe's own models, not Plex's. Plex is a source of
//! metadata behind the server; these types do not depend on it.
//!
//! Permissively licensed (MIT OR Apache-2.0) so that any client, open or
//! proprietary, can link it. It never depends on an AGPL server crate.

pub mod browse;
pub mod capabilities;
pub mod codec;
pub mod codec_string;
pub mod date;
pub mod id;
pub mod language;
pub mod media;
pub mod model;
pub mod platform;
pub mod preferences;
pub mod required;
pub mod role;
pub mod search;
pub mod serde_time;
#[cfg(test)]
mod test_support;
#[cfg(feature = "uniffi")]
mod uniffi_methods;
#[cfg(feature = "uniffi")]
mod uniffi_types;
pub mod wire;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

pub use browse::{
    Counted, DynamicRange, FilterPart, FilterValue, ItemFilter, ItemFilterValues, ItemQuery,
    ItemSort, Resolution, SortKey, WatchedState,
};
pub use capabilities::{CodecProbe, Decoders, candidate_codec_strings};
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
    Album, Artist, Artwork, ArtworkKind, Caption, CastMember, Episode, Item, Library, LibraryKind,
    LibraryServer, Marker, MarkerKind, Markers, Movie, PlaybackState, Progress, Season, Show,
    Track, YearsAired,
};
pub use platform::Platform;
pub use preferences::{
    AudioPreferences, BitmapPreference, ForcedPreference, SdhPreference, SubtitleMode,
    SubtitlePreferences, TrackPreferences,
};
pub use role::Role;
pub use search::{Person, PersonRole, SearchKind, SearchScope};

pub mod pin;

/// Password syntax shared by clients and servers.
pub mod password {
    /// The fewest characters a new password may have.
    pub const MIN_CHARS: usize = 8;
}

/// The domain every server's app passkeys are made under: a server's
/// relying party is `<server id>.spoolframe.app`. Every client and the
/// admin API use this one value.
pub const APP_RP_DOMAIN: &str = "spoolframe.app";
