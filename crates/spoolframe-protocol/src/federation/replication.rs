//! Replication (P8-02): what a paired server asks of the one whose
//! libraries are shared with it, and the answers. Only a live pair's
//! connection reaches these; anyone else's stream is reset.
//!
//! The server that shares (SA) keeps no change feed and no poller: the
//! one it shares with (SB) asks for each shared library with [`SyncLibrary`],
//! and SA walks the library then. It diffs what it finds against what it
//! last gave SB, kept per pair and library as a content hash per item
//! ([`RemoteItem::hash`]), and answers with the changes, or with the whole
//! library when it does not know SB's cursor. Either answer comes as
//! [`SyncPage`]s on the request's own stream, as many frames as it takes;
//! SB keeps none of it until the last page is read, and then names the
//! cursor in its next `SyncLibrary`.
//!
//! Every list here is bounded, and what a server reads of a page is cut
//! to the bounds ([`SyncPage::within_bounds`]): a longer list loses its
//! tail, a longer text its end.

use serde::{Deserialize, Serialize};

/// The most items one [`SyncPage`] carries; the rest are left out.
pub const MAX_PAGE_ITEMS: usize = 200;

/// The most ids of removed items one [`SyncPage`] carries.
pub const MAX_PAGE_REMOVED: usize = 2_000;

/// The most libraries a [`PeerReply::Libraries`](super::PeerReply) lists.
pub const MAX_SHARED_LIBRARIES: usize = 256;

/// The most artwork, external ids and genres one item carries.
pub const MAX_ITEM_ARTWORK: usize = 8;
pub const MAX_ITEM_EXTERNAL_IDS: usize = 16;
pub const MAX_ITEM_GENRES: usize = 16;

/// The longest title, name or short text an item carries, in characters.
pub const MAX_ITEM_TEXT: usize = 512;

/// The longest summary an item carries, in characters.
pub const MAX_ITEM_SUMMARY: usize = 8_192;

/// The largest image an [`ArtworkFetch`] is answered with, in bytes.
pub const MAX_ARTWORK_BYTES: usize = 16 << 20;

/// The largest reply to an [`ArtworkFetch`] a server reads, in bytes: the
/// image's [`MAX_ARTWORK_BYTES`] and room for its media type and the
/// reply around it. One announced longer is refused before any of it is
/// held, not read to the reply's own 64 MiB bound first (F8-12).
pub const MAX_ARTWORK_REPLY: usize = MAX_ARTWORK_BYTES + (64 << 10);

/// Ask for one shared library's changes since `since`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncLibrary {
    /// The library's id, as the sharing server knows it
    /// ([`SharedLibrary::id`]).
    pub library: String,
    /// The cursor the last whole answer gave; `None` the first time. One
    /// the sharing server does not know is answered with the whole
    /// library.
    #[serde(default)]
    pub since: Option<String>,
}

/// Ask for one image of a shared library's item, by the id an item's
/// [`RemoteArtwork`] gave. Answered with
/// [`PeerReply::Artwork`](super::PeerReply), written as a reply
/// ([`crate::codec::write_reply`]) rather than a frame, as an image may be
/// larger than one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtworkFetch {
    pub id: String,
}

/// What a library holds, as a sharing server says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteLibraryKind {
    Movies,
    Shows,
    Music,
    /// A kind this build does not know: not cached.
    #[serde(other)]
    Unknown,
}

impl RemoteLibraryKind {
    /// Its name on the wire; `None` for one this build does not know.
    pub fn as_str(self) -> Option<&'static str> {
        Some(match self {
            RemoteLibraryKind::Movies => "movies",
            RemoteLibraryKind::Shows => "shows",
            RemoteLibraryKind::Music => "music",
            RemoteLibraryKind::Unknown => return None,
        })
    }
}

/// A library shared with the server asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedLibrary {
    /// Its id on the sharing server.
    pub id: String,
    /// What the sharing server calls it.
    pub title: String,
    pub kind: RemoteLibraryKind,
}

/// What an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteKind {
    Movie,
    Show,
    Season,
    Episode,
    Artist,
    Album,
    Track,
    /// A kind this build does not know: left out.
    #[serde(other)]
    Unknown,
}

impl RemoteKind {
    /// Its name on the wire; `None` for one this build does not know.
    pub fn as_str(self) -> Option<&'static str> {
        Some(match self {
            RemoteKind::Movie => "movie",
            RemoteKind::Show => "show",
            RemoteKind::Season => "season",
            RemoteKind::Episode => "episode",
            RemoteKind::Artist => "artist",
            RemoteKind::Album => "album",
            RemoteKind::Track => "track",
            RemoteKind::Unknown => return None,
        })
    }
}

/// The role a piece of artwork plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteArtworkKind {
    Poster,
    Background,
    Thumb,
    /// A role this build does not know: left out.
    #[serde(other)]
    Unknown,
}

impl RemoteArtworkKind {
    /// Its name on the wire; `None` for one this build does not know.
    pub fn as_str(self) -> Option<&'static str> {
        Some(match self {
            RemoteArtworkKind::Poster => "poster",
            RemoteArtworkKind::Background => "background",
            RemoteArtworkKind::Thumb => "thumb",
            RemoteArtworkKind::Unknown => return None,
        })
    }
}

/// A piece of an item's artwork, fetched with [`ArtworkFetch`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteArtwork {
    /// Its id on the sharing server.
    pub id: String,
    pub kind: RemoteArtworkKind,
}

/// One item of a shared library, as the sharing server has it. Ids are
/// the sharing server's own; the server that caches it names them as its
/// origin's (`spoolframe_types::MediaId::federated`).
///
/// Its place in the hierarchy: a season's `parent` is its show; an
/// episode's `parent` its season and `grandparent` its show; an album's
/// `parent` its artist; a track's `parent` its album and `grandparent` its
/// artist. `index` is a season's number, an episode's, a track's;
/// `parent_index` an episode's season number, a track's disc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteItem {
    pub id: String,
    pub kind: RemoteKind,
    /// A title, or an artist's name.
    pub title: String,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub grandparent: Option<String>,
    /// The parent's title: a season's or episode's show, an album's
    /// artist, a track's album.
    #[serde(default)]
    pub parent_title: Option<String>,
    /// The grandparent's: a track's artist.
    #[serde(default)]
    pub grandparent_title: Option<String>,
    #[serde(default)]
    pub index: Option<u32>,
    #[serde(default)]
    pub parent_index: Option<u32>,
    #[serde(default)]
    pub year: Option<u16>,
    /// A film's release, an episode's air date, a show's last airing:
    /// `YYYY-MM-DD`.
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub content_rating: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// A show's season count, an album's track count, a track's album's.
    #[serde(default)]
    pub count: Option<u32>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub artwork: Vec<RemoteArtwork>,
    /// Its stable ids outside the sharing server's keys: TMDB, IMDb,
    /// TVDB, MusicBrainz, Plex's guid (`tmdb://603`). What progress per
    /// title (P8-03) and merged search (P8-05) match on.
    #[serde(default)]
    pub external_ids: Vec<String>,
    /// What a library is sorted and filtered by
    /// ([`spoolframe_types::browse`]), each left out when the sharing
    /// server has none: one from before them sends none, and the item
    /// hashes as it did. A film's, a show's or an artist's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_title: Option<String>,
    /// The day it first came out, `YYYY-MM-DD`: a film's release, a
    /// show's first airing (where `date` is its last).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released: Option<String>,
    /// When it was added to the sharing server's catalog, Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<i64>,
    /// The critic rating, in tenths: 0 to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating_tenths: Option<u16>,
    /// The audience rating, in tenths: 0 to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience_rating_tenths: Option<u16>,
    /// A film's file size, its first version's, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// A film's resolution class, its first version's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<spoolframe_types::Resolution>,
    /// A film's dynamic range, its first version's: `sdr`, `hdr` (not
    /// Dolby Vision) or `dolby_vision`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic_range: Option<spoolframe_types::DynamicRange>,
    /// The content hash ([`RemoteItem::digest`]) the sharing server
    /// diffed by: unchanged, unchanged item.
    pub hash: String,
}

/// `text` cut to `max` characters.
fn cut(text: &mut String, max: usize) {
    if let Some((at, _)) = text.char_indices().nth(max) {
        text.truncate(at);
    }
}

fn cut_option(text: &mut Option<String>, max: usize) {
    if let Some(text) = text {
        cut(text, max);
    }
}

impl RemoteItem {
    /// The content hash of everything but `hash`: blake3, hex. Two items
    /// with the same hash are the same item, as far as a cache can tell.
    pub fn digest(&self) -> String {
        let unhashed = RemoteItem {
            hash: String::new(),
            ..self.clone()
        };
        let bytes = crate::codec::encode(&unhashed).expect("an item encodes");
        blake3::hash(&bytes).to_hex().to_string()
    }

    /// This item with `hash` set to its [`RemoteItem::digest`].
    pub fn hashed(mut self) -> Self {
        self.hash = self.digest();
        self
    }

    /// This item cut to the bounds: what a server keeps of one.
    pub fn within_bounds(mut self) -> Self {
        for text in [&mut self.id, &mut self.title, &mut self.hash] {
            cut(text, MAX_ITEM_TEXT);
        }
        for text in [
            &mut self.parent,
            &mut self.grandparent,
            &mut self.parent_title,
            &mut self.grandparent_title,
            &mut self.date,
            &mut self.content_rating,
            &mut self.sort_title,
            &mut self.released,
        ] {
            cut_option(text, MAX_ITEM_TEXT);
        }
        cut_option(&mut self.summary, MAX_ITEM_SUMMARY);
        self.genres.truncate(MAX_ITEM_GENRES);
        self.artwork.truncate(MAX_ITEM_ARTWORK);
        self.external_ids.truncate(MAX_ITEM_EXTERNAL_IDS);
        for text in self
            .genres
            .iter_mut()
            .chain(self.external_ids.iter_mut())
            .chain(self.artwork.iter_mut().map(|a| &mut a.id))
        {
            cut(text, MAX_ITEM_TEXT);
        }
        self
    }
}

/// What a sync's pages hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// What changed since the cursor asked about: items added or changed,
    /// and the ids of those gone. What is not named is as it was.
    Changes,
    /// The whole library: what is not named is gone.
    Snapshot,
    /// A mode this build does not know: the answer is not used.
    #[serde(other)]
    Unknown,
}

/// One page of the answer to [`SyncLibrary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPage {
    pub mode: SyncMode,
    /// The cursor to name as `since` next time, once every page is read:
    /// the same on each page.
    pub cursor: String,
    /// At most [`MAX_PAGE_ITEMS`].
    #[serde(default)]
    pub items: Vec<RemoteItem>,
    /// Ids of items gone, with [`SyncMode::Changes`]; at most
    /// [`MAX_PAGE_REMOVED`].
    #[serde(default)]
    pub removed: Vec<String>,
    /// Another page follows on this stream.
    #[serde(default)]
    pub more: bool,
}

impl SyncPage {
    /// This page cut to the bounds: what a server keeps of one.
    pub fn within_bounds(mut self) -> Self {
        cut(&mut self.cursor, MAX_ITEM_TEXT);
        self.items.truncate(MAX_PAGE_ITEMS);
        self.items = self
            .items
            .into_iter()
            .map(RemoteItem::within_bounds)
            .collect();
        self.removed.truncate(MAX_PAGE_REMOVED);
        for id in &mut self.removed {
            cut(id, MAX_ITEM_TEXT);
        }
        self
    }
}

/// The libraries listed cut to [`MAX_SHARED_LIBRARIES`], their texts to
/// [`MAX_ITEM_TEXT`]: what a server keeps of a listing.
pub fn libraries_within_bounds(mut libraries: Vec<SharedLibrary>) -> Vec<SharedLibrary> {
    libraries.truncate(MAX_SHARED_LIBRARIES);
    for library in &mut libraries {
        cut(&mut library.id, MAX_ITEM_TEXT);
        cut(&mut library.title, MAX_ITEM_TEXT);
    }
    libraries
}
