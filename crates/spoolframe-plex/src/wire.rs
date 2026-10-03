//! The parts of PMS JSON responses that Spoolframe reads.
//!
//! Only the fields the conversion uses are declared; everything else Plex
//! sends (ratings, guids, view counts, streams…) is ignored on purpose, so
//! Plex adding fields never breaks parsing and Plex-only data never leaks
//! into the media model.

use serde::Deserialize;

/// Every PMS JSON response is `{"MediaContainer": {...}}`.
#[derive(Debug, Deserialize)]
pub struct Envelope {
    #[serde(rename = "MediaContainer")]
    pub container: MediaContainer,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaContainer {
    /// Present on paginated listings: the size of the whole listing.
    pub total_size: Option<u32>,
    /// The library the items belong to, on item and listing responses.
    /// Library-scoped path mappings match on it.
    pub library_section_title: Option<String>,
    /// What the PMS calls itself, on its root (`/`).
    pub friendly_name: Option<String>,
    #[serde(rename = "Directory", default)]
    pub directories: Vec<Directory>,
    #[serde(rename = "Metadata", default)]
    pub metadata: Vec<Metadata>,
    #[serde(rename = "Hub", default)]
    pub hubs: Vec<Hub>,
}

/// A library section in `/library/sections`. Other listings also use
/// `Directory` for navigation entries (a show's "All episodes"); those have
/// no `type` and are not read.
#[derive(Debug, Deserialize)]
pub struct Directory {
    pub key: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub title: String,
}

/// One group of `/hubs/search` results.
#[derive(Debug, Deserialize)]
pub struct Hub {
    #[serde(rename = "Metadata", default)]
    pub metadata: Vec<Metadata>,
}

/// An item of any kind; which fields are set depends on `type`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    pub rating_key: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub year: Option<u16>,
    pub summary: Option<String>,
    /// Milliseconds.
    pub duration: Option<u64>,
    /// Season, episode or track number.
    pub index: Option<u32>,
    /// The parent's number: an episode's season, a track's disc.
    pub parent_index: Option<u32>,
    pub parent_rating_key: Option<String>,
    pub grandparent_rating_key: Option<String>,
    /// An episode's show's title.
    pub grandparent_title: Option<String>,
    /// A season's show's title, an album's artist's name.
    pub parent_title: Option<String>,
    /// The parent's and grandparent's images: an episode's season poster
    /// and show poster.
    pub parent_thumb: Option<String>,
    pub grandparent_thumb: Option<String>,
    pub thumb: Option<String>,
    pub art: Option<String>,
    #[serde(rename = "Media", default)]
    pub media: Vec<Media>,
    /// Plex's own stable id, `plex://movie/…`.
    pub guid: Option<String>,
    /// Other agents' ids (`imdb://…`); only on single-item responses, or
    /// listings asked with `includeGuids=1`.
    #[serde(rename = "Guid", default)]
    pub guids: Vec<GuidRef>,
    /// The token's account's views: how many times it was watched to the
    /// end, where it stopped (ms), and when it was last viewed (Unix s).
    pub view_count: Option<u32>,
    pub view_offset: Option<u64>,
    pub last_viewed_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct GuidRef {
    pub id: String,
}

/// One version of an item's media.
#[derive(Debug, Deserialize)]
pub struct Media {
    #[serde(rename = "Part", default)]
    pub parts: Vec<Part>,
}

/// One file of a version.
#[derive(Debug, Deserialize)]
pub struct Part {
    pub file: Option<String>,
}
