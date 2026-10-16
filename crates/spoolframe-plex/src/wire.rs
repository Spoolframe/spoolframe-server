//! The parts of PMS JSON responses that Spoolframe reads.
//!
//! Only the fields the conversion uses are declared; everything else Plex
//! sends (ratings, guids, view counts, streams…) is ignored on purpose, so
//! Plex adding fields never breaks parsing and Plex-only data never leaks
//! into the media model.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use spoolframe_types::CalendarDate;

/// A field read only for an admin's look at an item (P3-16): one Plex
/// sends in another shape is left out rather than failing the item.
fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Option<T>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(value).ok())
}

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
    /// That library's key, on the same responses.
    #[serde(rename = "librarySectionID", default)]
    pub library_section_id: Option<SectionKey>,
    /// What the PMS calls itself, on its root (`/`).
    pub friendly_name: Option<String>,
    /// The PMS's version, on its root (`/`).
    pub version: Option<String>,
    /// The PMS's own id, on `/identity`: what plex.tv's resources list
    /// knows it by (P3-12).
    pub machine_identifier: Option<String>,
    #[serde(rename = "Directory", default)]
    pub directories: Vec<Directory>,
    #[serde(rename = "Metadata", default)]
    pub metadata: Vec<Metadata>,
    #[serde(rename = "Hub", default)]
    pub hubs: Vec<Hub>,
}

/// A library section's key. Plex sends it as a string in
/// `/library/sections` and as a number on items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionKey(pub String);

impl<'de> Deserialize<'de> for SectionKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Number(u64),
            Text(String),
        }
        Ok(SectionKey(match Either::deserialize(d)? {
            Either::Number(n) => n.to_string(),
            Either::Text(s) => s,
        }))
    }
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
    /// A library section's folders, as Plex sees them.
    #[serde(rename = "Location", default)]
    pub locations: Vec<Location>,
}

/// One folder of a library section.
#[derive(Debug, Deserialize)]
pub struct Location {
    pub path: String,
}

/// One group of `/hubs/search` results.
#[derive(Debug, Deserialize)]
pub struct Hub {
    /// What the hub holds: `movie`, `episode`, `actor`, `director`…
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// The people in an `actor` or `director` hub.
    #[serde(rename = "Directory", default)]
    pub tags: Vec<HubTag>,
}

/// A person in a search hub: a tag, with the section Plex found them in.
/// Plex lists one person once per section.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubTag {
    /// Their name.
    pub tag: String,
    /// The tag's id: what a listing is filtered by (`actor=<id>`).
    pub id: u64,
    /// How many items in that section they are in.
    pub count: Option<u32>,
    #[serde(rename = "librarySectionID", default)]
    pub library_section_id: Option<SectionKey>,
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
    /// The day it was first released or aired, `YYYY-MM-DD`; on
    /// episodes, in listings too. A date in another shape is left out
    /// rather than losing the item.
    #[serde(default, deserialize_with = "lenient")]
    pub originally_available_at: Option<CalendarDate>,
    pub summary: Option<String>,
    /// The age rating: `R`, `TV-MA`. Plex sends it on movies, shows and
    /// episodes, in listings too.
    pub content_rating: Option<String>,
    /// Milliseconds.
    pub duration: Option<u64>,
    /// Season, episode or track number.
    pub index: Option<u32>,
    /// The parent's number: an episode's season, a track's disc.
    pub parent_index: Option<u32>,
    pub parent_rating_key: Option<String>,
    pub grandparent_rating_key: Option<String>,
    /// An episode's show's title, a track's album artist's name.
    pub grandparent_title: Option<String>,
    /// A season's show's title, an album's artist's name, a track's
    /// album's title.
    pub parent_title: Option<String>,
    /// A track's own artist, from its tags, when it is not the album's:
    /// "Adele with Chris Stapleton" on an Adele album.
    pub original_title: Option<String>,
    /// The parent's and grandparent's images: an episode's season poster
    /// and show poster, a track's album cover and artist picture.
    pub parent_thumb: Option<String>,
    pub grandparent_thumb: Option<String>,
    pub thumb: Option<String>,
    pub art: Option<String>,
    #[serde(rename = "Media", default)]
    pub media: Vec<Media>,
    /// Plex's own stable id, `plex://movie/…`.
    pub guid: Option<String>,
    /// A playlist's: `video`, `audio` or `photo` (PX-40).
    pub playlist_type: Option<String>,
    /// A playlist made by rules rather than by hand (PX-40).
    #[serde(default, deserialize_with = "lenient")]
    pub smart: Option<bool>,
    /// Other agents' ids (`imdb://…`); only on single-item responses, or
    /// listings asked with `includeGuids=1`.
    #[serde(rename = "Guid", default)]
    pub guids: Vec<GuidRef>,
    /// The token's account's views: how many times it was watched to the
    /// end, where it stopped (ms), and when it was last viewed (Unix s).
    pub view_count: Option<u32>,
    pub view_offset: Option<u64>,
    pub last_viewed_at: Option<i64>,
    /// When it came into the library (Unix s): recently added from several
    /// Plex servers is merged by it (P3-11).
    pub added_at: Option<i64>,
    /// When Plex last changed it (Unix s), for an admin (P3-16).
    #[serde(default, deserialize_with = "lenient")]
    pub updated_at: Option<i64>,
    /// A show's or season's episodes, an album's tracks.
    #[serde(default, deserialize_with = "lenient")]
    pub leaf_count: Option<u32>,
    /// A show's seasons, specials counted; also on clients' shows.
    #[serde(default, deserialize_with = "lenient")]
    pub child_count: Option<u32>,
    /// A show's genres. Listings and search give the first two; the
    /// show's own item gives all.
    #[serde(rename = "Genre", default)]
    pub genres: Vec<Tag>,
    /// A show's or an artist's folders; only on single-item responses.
    #[serde(rename = "Location", default)]
    pub locations: Vec<Location>,
    /// The library it is in. Listings that mix libraries (recently added,
    /// search) have it on every item.
    #[serde(rename = "librarySectionID", default)]
    pub library_section_id: Option<SectionKey>,
    /// The intros, credits and the like Plex found in the item; only on
    /// single-item responses asked with `includeMarkers=1`, and absent
    /// without Plex Pass or with detection off. Each is read as a
    /// [`Marker`] on its own (`convert::markers`): markers are extra, and
    /// one in another shape must not make the item unreadable.
    #[serde(rename = "Marker", default)]
    pub markers: Vec<serde_json::Value>,
    /// The actors, in billing order, and the directors. An item's own
    /// response has each with its tag id, the part played and a photo;
    /// listings name only the first few (three actors, a director), by
    /// name alone. Each is read as a [`Person`] on its own, as markers
    /// are.
    #[serde(rename = "Role", default)]
    pub roles: Vec<serde_json::Value>,
    #[serde(rename = "Director", default)]
    pub directors: Vec<serde_json::Value>,
}

/// Someone in an item's credits, as an item's own response has them.
#[derive(Debug, Deserialize)]
pub struct Person {
    /// The tag's id: what a listing is filtered by (`actor=<id>`), as a
    /// search hub's person has it.
    pub id: u64,
    /// Their name.
    pub tag: String,
    /// The part an actor plays.
    #[serde(default, deserialize_with = "lenient")]
    pub role: Option<String>,
    /// A photo's URL; Plex's own agent's are on
    /// `https://metadata-static.plex.tv/`.
    #[serde(default, deserialize_with = "lenient")]
    pub thumb: Option<String>,
}

/// A stretch of an item Plex detected.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Marker {
    /// `intro`, `credits`, and others (`commercial`).
    #[serde(rename = "type")]
    pub kind: String,
    /// Milliseconds.
    pub start_time_offset: u64,
    /// Milliseconds; on a `final` marker it can be 1 ms past the item's
    /// `duration`.
    pub end_time_offset: u64,
    /// Set on credits that run to the end of the item.
    #[serde(rename = "final", default)]
    pub is_final: bool,
}

/// A tag on an item (a genre); only its name is read.
#[derive(Debug, Deserialize)]
pub struct Tag {
    pub tag: String,
}

#[derive(Debug, Deserialize)]
pub struct GuidRef {
    pub id: String,
}

/// One version of an item's media. Everything but its parts is only for
/// an admin's look at it (P3-16).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Media {
    #[serde(rename = "Part", default)]
    pub parts: Vec<Part>,
    #[serde(default, deserialize_with = "lenient")]
    pub id: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    pub container: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub video_codec: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub video_resolution: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub video_profile: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub audio_codec: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub audio_channels: Option<u32>,
    /// kbit/s.
    #[serde(default, deserialize_with = "lenient")]
    pub bitrate: Option<u64>,
    /// Milliseconds.
    #[serde(default, deserialize_with = "lenient")]
    pub duration: Option<u64>,
}

/// One file of a version.
#[derive(Debug, Deserialize)]
pub struct Part {
    pub id: Option<u64>,
    pub file: Option<String>,
    /// Bytes; for an admin (P3-16).
    #[serde(default, deserialize_with = "lenient")]
    pub size: Option<u64>,
    /// Milliseconds; for an admin (P3-16).
    #[serde(default, deserialize_with = "lenient")]
    pub duration: Option<u64>,
    /// The preview indexes Plex made of it: `sd` when it has its "video
    /// preview thumbnails" (P3-13).
    pub indexes: Option<String>,
    /// Its streams; only on items asked for by key, never in listings.
    #[serde(rename = "Stream", default)]
    pub streams: Vec<Stream>,
}

/// One stream of a part. Only a track's audio format is read from it, and
/// a field in another shape is left out rather than failing the item.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stream {
    /// 1 video, 2 audio, 3 subtitles, 4 lyrics.
    #[serde(default, deserialize_with = "lenient")]
    pub stream_type: Option<u32>,
    #[serde(default, deserialize_with = "lenient")]
    pub bit_depth: Option<u32>,
    /// Hz.
    #[serde(default, deserialize_with = "lenient")]
    pub sampling_rate: Option<u32>,
}

impl Stream {
    pub const AUDIO: u32 = 2;
}
