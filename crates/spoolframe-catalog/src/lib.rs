//! The `Catalog` trait: where the server gets its libraries and items from
//! (ARCHITECTURE §4).
//!
//! Plex is the only catalog today (`spoolframe-plex`); a `NativeCatalog`
//! replaces it later (P9). The trait lives in its own crate so that both
//! can implement it and the server can hold either, without the server and
//! a catalog depending on each other.

pub mod inspect;
mod search;

pub use inspect::{
    Browse, FileCheck, Inspect, Inspected, LibraryCheck, MappingUsed, MissingFile, Row, RowFilter,
    RowPage, Version, VersionPart,
};
pub use search::{Found, FoundGroup, SearchQuery, interleave};

use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;
use spoolframe_types::{ArtworkId, Item, Library, LibraryId, Marker, MediaId, MediaKind, PersonId};

/// Which slice of a library to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    /// How many items to skip.
    pub offset: u32,
    /// The most items to return.
    pub limit: u32,
}

/// One page of a library listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemPage {
    pub items: Vec<Item>,
    /// How many items the whole library has.
    pub total: u32,
}

/// An item and its direct children: a show's seasons, a season's
/// episodes, an artist's albums, an album's tracks. Movies, episodes and
/// tracks have none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemWithChildren {
    pub item: Item,
    pub children: Vec<Item>,
    /// The item's stable identities outside this catalog's own keys
    /// (`plex://movie/…`, `imdb://tt…`, `tmdb://…`). Server side only:
    /// watch state is re-linked through them when an item's `MediaId`
    /// changes (a Plex rebuild, or the move to a native catalog).
    pub external_ids: Vec<String>,
    /// The item's intro and credits (P2-10), in order of their start.
    /// Empty when the catalog knows of none; never `MarkerKind::Unknown`.
    pub markers: Vec<Marker>,
}

/// Why a catalog call failed. A catalog never answers a failure with an
/// empty result.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// The catalog has no such library or item.
    #[error("{0} not found")]
    NotFound(String),
    /// The ID belongs to another catalog's namespace.
    #[error("{0} is not an ID this catalog serves")]
    ForeignId(String),
    /// The item has no media file of its own (a show, season, artist or
    /// album).
    #[error("{0} has no media file")]
    NotPlayable(MediaId),
    /// The item's media file, mapped to this host, cannot be read.
    #[error("{0}")]
    MediaFileUnavailable(Box<UnavailableFile>),
    /// The catalog's backend could not be reached.
    #[error("cannot reach {url} ({call}): {source}")]
    Unreachable {
        url: String,
        call: String,
        source: Box<dyn Error + Send + Sync>,
    },
    /// The backend answered, but not with something usable.
    #[error("{url} ({call}){}: {message}", item.as_ref().map(|i| format!(", item {i}")).unwrap_or_default())]
    Backend {
        url: String,
        call: String,
        item: Option<String>,
        message: String,
    },
    /// There is no catalog yet: the server has not been set up (P3-10).
    #[error("this server is not set up yet: it has no library")]
    NotSetUp,
    /// The item is in a library the server does not serve now: one never
    /// added, or removed (P3-11), or on a Plex server no longer configured.
    /// Clients see it as not found; its watch state is kept, and nothing
    /// re-links it elsewhere, so adding the library back restores it.
    #[error("{0} is in a library this server does not serve")]
    NotServed(String),
}

/// A media file that cannot be read on this host. Names the path the
/// catalog reported, the mapping applied and the result, so the admin can
/// fix the mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableFile {
    pub id: MediaId,
    /// The path as the catalog reported it.
    pub reported: PathBuf,
    /// The mapping rule applied, or `None` when none matched.
    pub mapping: Option<String>,
    /// The path probed on this host.
    pub local: PathBuf,
    pub reason: String,
}

impl fmt::Display for UnavailableFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: media file {} is not readable ({}); catalog path {}, mapping {}",
            self.id,
            self.local.display(),
            self.reason,
            self.reported.display(),
            self.mapping.as_deref().unwrap_or("none")
        )
    }
}

/// A source of libraries and items.
#[async_trait]
pub trait Catalog: Send + Sync {
    /// Every library the server offers.
    async fn libraries(&self) -> Result<Vec<Library>, CatalogError>;

    /// The top-level items of a library (movies, shows or artists), one
    /// page at a time.
    async fn items(&self, library: &LibraryId, page: Page) -> Result<ItemPage, CatalogError>;

    /// One item, with its direct children.
    async fn item(&self, id: &MediaId) -> Result<ItemWithChildren, CatalogError>;

    /// Every episode of the show `show`, specials included, in no order
    /// promised: what [`Catalog::item`] finds walking the show's seasons.
    /// A backend that can list them at once does so in one request.
    async fn episodes(&self, show: &MediaId) -> Result<Vec<Item>, CatalogError> {
        let mut episodes = Vec::new();
        for season in self.item(show).await?.children {
            if let Item::Season(season) = season {
                episodes.extend(
                    self.item(&season.id)
                        .await?
                        .children
                        .into_iter()
                        .filter(|e| matches!(e, Item::Episode(_))),
                );
            }
        }
        Ok(episodes)
    }

    /// Every track of music library `library`, in no order promised:
    /// what the built-in All playlist (PX-43) is made of. Walks its
    /// artists' albums by default; a backend that can list a library's
    /// tracks at once does so, a page of requests rather than one per
    /// album.
    async fn tracks(&self, library: &LibraryId) -> Result<Vec<Item>, CatalogError> {
        const PAGE: u32 = 200;
        let mut artists = Vec::new();
        loop {
            let page = self
                .items(
                    library,
                    Page {
                        offset: artists.len() as u32,
                        limit: PAGE,
                    },
                )
                .await?;
            let got = page.items.len();
            artists.extend(page.items);
            if got == 0 || artists.len() >= page.total as usize {
                break;
            }
        }
        let mut tracks = Vec::new();
        for artist in artists.iter().filter(|a| matches!(a, Item::Artist(_))) {
            for album in self.item(artist.id()).await?.children {
                if let Item::Album(album) = album {
                    tracks.extend(
                        self.item(&album.id)
                            .await?
                            .children
                            .into_iter()
                            .filter(|t| matches!(t, Item::Track(_))),
                    );
                }
            }
        }
        Ok(tracks)
    }

    /// The stable identities of the item now under `id`, as
    /// [`ItemWithChildren::external_ids`], without its children or
    /// markers: the cheapest way to ask what a key holds. `NotFound` when
    /// the catalog has no item of that kind under that key, also when the
    /// key now names an item of another kind.
    async fn external_ids(&self, id: &MediaId) -> Result<Vec<String>, CatalogError>;

    /// The item of `kind` that has one of `external_ids` now, in the
    /// libraries served: how something kept by an item's stable ids (a
    /// playlist's entry, PX-40) finds it again after its key changed.
    /// `None` when no item has them, or when this catalog cannot look
    /// items up by them. An error is "cannot say": the caller keeps what
    /// it has.
    async fn find_external(
        &self,
        _kind: MediaKind,
        _external_ids: &[String],
    ) -> Result<Option<MediaId>, CatalogError> {
        Ok(None)
    }

    /// Each kind `query` asks for, in the libraries it covers (P3-09).
    /// The catalog's backend applies the scope; nothing is filtered out
    /// afterwards.
    async fn search(&self, query: &SearchQuery) -> Result<Found, CatalogError>;

    /// The items a person [`Catalog::search`] found is in, in the
    /// libraries served (P3-09).
    async fn person_items(&self, id: &PersonId) -> Result<Vec<Item>, CatalogError>;

    /// The local path of the item's media file: what the catalog reports,
    /// mapped to this host, and checked to be a readable file.
    async fn media_path(&self, id: &MediaId) -> Result<PathBuf, CatalogError>;

    /// The most recently added items, newest first, at most `limit`.
    async fn recently_added(&self, limit: u32) -> Result<Vec<Item>, CatalogError>;

    /// Check the image's owning item against the libraries currently served.
    /// Call before serving cached bytes too; a cached image grants no access.
    async fn authorize_artwork(&self, id: &ArtworkId) -> Result<(), CatalogError>;

    /// The image an `ArtworkId` names, at the size the catalog has it.
    /// Never anything but an image.
    async fn artwork(&self, id: &ArtworkId) -> Result<Image, CatalogError>;

    /// Frame previews the catalog already made of the item's media file
    /// (P3-13): Plex's "video preview thumbnails". `None` when it has
    /// none, and the server makes its own; a catalog that never makes any
    /// keeps this default.
    async fn preview_frames(&self, _id: &MediaId) -> Result<Option<PreviewFrames>, CatalogError> {
        Ok(None)
    }
}

/// A catalog's own frame previews of a file: pictures at times through
/// it, in order.
#[derive(Clone, PartialEq, Eq)]
pub struct PreviewFrames {
    pub frames: Vec<PreviewFrame>,
}

/// One picture of a [`PreviewFrames`].
#[derive(Clone, PartialEq, Eq)]
pub struct PreviewFrame {
    /// The time in the file it shows.
    pub at_ms: u64,
    /// A JPEG.
    pub jpeg: Vec<u8>,
}

impl fmt::Debug for PreviewFrames {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreviewFrames")
            .field("frames", &self.frames.len())
            .field("last_ms", &self.frames.last().map(|l| l.at_ms))
            .finish()
    }
}

impl fmt::Debug for PreviewFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreviewFrame")
            .field("at_ms", &self.at_ms)
            .field("jpeg", &self.jpeg.len())
            .finish()
    }
}

/// An image and its media type.
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    pub bytes: Vec<u8>,
    /// e.g. `image/jpeg`.
    pub content_type: String,
}

impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Image")
            .field("content_type", &self.content_type)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backend_error_names_the_url_call_and_item() {
        let err = CatalogError::Backend {
            url: "http://nas:32400".into(),
            call: "GET /library/metadata/7".into(),
            item: Some("plex:movie:7".into()),
            message: "unexpected type \"clip\"".into(),
        };
        assert_eq!(
            err.to_string(),
            "http://nas:32400 (GET /library/metadata/7), item plex:movie:7: unexpected type \"clip\""
        );
    }

    #[test]
    fn an_unreachable_error_names_the_url() {
        let err = CatalogError::Unreachable {
            url: "http://nas:32400".into(),
            call: "GET /library/sections".into(),
            source: "connection refused".into(),
        };
        assert!(err.to_string().contains("http://nas:32400"), "{err}");
    }
}
