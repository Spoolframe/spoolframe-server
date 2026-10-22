//! What an admin sees of a catalog (P3-16), beyond what clients get: each
//! item's match, its files where the catalog reports them and where they
//! are on this host, and a check of every file in a library (P1-04).
//!
//! Read-only. A catalog that has an admin's look at it implements
//! [`Inspect`] as well as [`Catalog`](crate::Catalog); the admin pages read through it, so
//! they work for Plex libraries now and native ones later (P9).

use std::path::PathBuf;

use async_trait::async_trait;
use spoolframe_types::{Item, LibraryId, MediaId, MediaKind};

use crate::{CatalogError, ItemWithChildren, Page};

/// Which rows of a library to list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Browse {
    /// The kinds listed, one after the other in this order: `[Show]` for a
    /// TV library's shows, `[Show, Episode]` to search both.
    pub kinds: Vec<MediaKind>,
    /// Only items whose titles match, as the catalog's search matches
    /// them (P3-09).
    pub text: Option<String>,
    pub filter: Option<RowFilter>,
    pub page: Page,
}

/// A quick filter of a library's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowFilter {
    /// Items the catalog's backend has not matched to an agent.
    Unmatched,
    /// Items with no artwork at all: clients show a placeholder.
    NoArtwork,
}

/// One page of rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowPage {
    pub rows: Vec<Row>,
    /// How many rows the whole listing has.
    pub total: u32,
}

/// An item as a library listing shows it to an admin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// As clients get it.
    pub item: Item,
    /// The backend's own id for it: Plex's `guid`.
    pub guid: Option<String>,
    /// Other agents' ids (`imdb://…`, `tmdb://…`, `tvdb://…`).
    pub agents: Vec<String>,
    /// Unix seconds.
    pub added_at: Option<i64>,
    pub updated_at: Option<i64>,
    /// A show's seasons, an album's tracks: its direct children.
    pub children: Option<u32>,
    /// A show's or season's episodes.
    pub leaves: Option<u32>,
    /// The files a playback reads, checked now: a movie's, an episode's or
    /// a track's. Empty for a show, season, artist or album.
    pub files: Vec<FileCheck>,
}

impl Row {
    /// Whether the backend matched it to an agent.
    pub fn matched(&self) -> bool {
        is_matched(self.guid.as_deref())
    }
}

/// Whether an item with backend id `guid` is matched: Plex gives
/// unmatched items a `local://` id, or the "none" agent's.
pub fn is_matched(guid: Option<&str>) -> bool {
    guid.is_some_and(|g| {
        !g.is_empty() && !g.starts_with("local://") && !g.starts_with("com.plexapp.agents.none")
    })
}

/// One media file: where the catalog reports it, where it is on this
/// host, and whether it can be read there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCheck {
    /// As the catalog reports it.
    pub reported: PathBuf,
    /// The path mapping used, `from` and `to`; `None` when none matched
    /// and the reported path is used as it is.
    pub mapping: Option<MappingUsed>,
    /// Where it was looked for on this host.
    pub local: PathBuf,
    /// Why it cannot be read there; `None` when it can.
    pub problem: Option<String>,
}

impl FileCheck {
    pub fn found(&self) -> bool {
        self.problem.is_none()
    }
}

/// A path mapping rule, as applied to a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingUsed {
    /// The library it is for; `None` for every library.
    pub library: Option<String>,
    pub from: PathBuf,
    pub to: PathBuf,
}

/// One item, as an admin inspects it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspected {
    /// As [`crate::Catalog::item`] gives it to clients.
    pub found: ItemWithChildren,
    /// The library it is in, when the catalog says.
    pub library: Option<LibraryId>,
    /// Its show and season, or its artist and album, outermost first.
    pub ancestors: Vec<(MediaId, String)>,
    pub guid: Option<String>,
    pub added_at: Option<i64>,
    pub updated_at: Option<i64>,
    /// A show's or an artist's folders, as the catalog reports them.
    pub folders: Vec<PathBuf>,
    /// Each version of its media, the one played first.
    pub versions: Vec<Version>,
    /// Its children as rows: what an item's Children tab lists.
    pub children: Vec<Row>,
}

impl Inspected {
    pub fn matched(&self) -> bool {
        is_matched(self.guid.as_deref())
    }
}

/// One version of an item's media, as the catalog describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// The backend's id for it: Plex's media id.
    pub id: Option<u64>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    /// `1080`, `4k`, `sd`, as the backend says it.
    pub video_resolution: Option<String>,
    /// `main 10`, `high`.
    pub video_profile: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<u32>,
    /// kbit/s.
    pub bitrate: Option<u64>,
    pub duration_ms: Option<u64>,
    pub parts: Vec<VersionPart>,
}

/// One file of a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionPart {
    /// The backend's id for it: Plex's part id.
    pub id: Option<u64>,
    /// Bytes.
    pub size: Option<u64>,
    pub duration_ms: Option<u64>,
    pub file: FileCheck,
}

/// Every media file of a library, checked (P1-04): what `plex
/// check-paths` reports, with each file that fails and its item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryCheck {
    /// Files this host can read.
    pub ok: u64,
    /// The files it cannot, in the catalog's order.
    pub missing: Vec<MissingFile>,
}

/// A file a library check found missing, and its item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingFile {
    /// A movie, an episode or a track, as a listing shows it, with all of
    /// its files as the check found them. An episode's item knows its show
    /// and season, a track's its album and artist.
    pub row: Row,
    /// The one that is missing.
    pub file: FileCheck,
}

impl LibraryCheck {
    /// How many missing files are `id`'s own or its descendants'.
    pub fn missing_under(&self, id: &MediaId) -> u64 {
        let count = self
            .missing
            .iter()
            .filter(|m| is_under(&m.row.item, id))
            .count();
        u64::try_from(count).unwrap_or(u64::MAX)
    }
}

/// Whether `item` is `id` or one of its descendants.
pub fn is_under(item: &Item, id: &MediaId) -> bool {
    if item.id() == id {
        return true;
    }
    match item {
        Item::Episode(e) => e.show_id == *id || e.season_id == *id,
        Item::Season(s) => s.show_id == *id,
        Item::Track(t) => t.album_id == *id || t.artist_id == *id,
        Item::Album(a) => a.artist_id == *id,
        _ => false,
    }
}

/// A catalog an admin can look into.
#[async_trait]
pub trait Inspect: Send + Sync {
    /// One page of a library's rows, as `query` asks.
    async fn browse(&self, library: &LibraryId, query: &Browse) -> Result<RowPage, CatalogError>;

    /// How many items of `kind` the library has.
    async fn count(&self, library: &LibraryId, kind: MediaKind) -> Result<u32, CatalogError>;

    /// The rows of an item's direct children: a show's seasons, a season's
    /// episodes, an album's tracks.
    async fn child_rows(&self, id: &MediaId) -> Result<Vec<Row>, CatalogError>;

    /// One item, with its match, files and children.
    async fn inspect(&self, id: &MediaId) -> Result<Inspected, CatalogError>;

    /// Every media file of a library, checked: the same check `plex
    /// check-paths` makes.
    async fn check_library(&self, library: &LibraryId) -> Result<LibraryCheck, CatalogError>;

    /// Where the item opens in the backend's own app, when one can be
    /// named.
    async fn backend_link(&self, id: &MediaId) -> Result<Option<String>, CatalogError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::{Episode, Namespace, PlaybackState};

    fn id(kind: MediaKind, key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, kind, key).unwrap()
    }

    #[test]
    fn plex_s_local_and_none_ids_are_unmatched() {
        assert!(is_matched(Some("plex://movie/5d776c85fb0d55001f589f07")));
        assert!(is_matched(Some("com.plexapp.agents.imdb://tt0078748")));
        assert!(!is_matched(Some("local://144791")));
        assert!(!is_matched(Some("com.plexapp.agents.none://abc")));
        assert!(!is_matched(None));
    }

    #[test]
    fn an_episode_is_under_its_season_and_show() {
        let episode = Item::Episode(Episode {
            id: id(MediaKind::Episode, "3"),
            show_id: id(MediaKind::Show, "1"),
            show_title: None,
            season_id: id(MediaKind::Season, "2"),
            season: 1,
            episode: 1,
            title: "Pilot".into(),
            summary: None,
            content_rating: None,
            year: None,
            air_date: None,
            duration: None,
            cast: Vec::new(),
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        });
        assert!(is_under(&episode, &id(MediaKind::Show, "1")));
        assert!(is_under(&episode, &id(MediaKind::Season, "2")));
        assert!(is_under(&episode, &id(MediaKind::Episode, "3")));
        assert!(!is_under(&episode, &id(MediaKind::Season, "9")));
    }
}
