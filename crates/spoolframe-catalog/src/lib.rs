//! The `Catalog` trait: where the server gets its libraries and items from
//! (ARCHITECTURE §4).
//!
//! Plex is the only catalog today (`spoolframe-plex`); a `NativeCatalog`
//! replaces it later (P9). The trait lives in its own crate so that both
//! can implement it and the server can hold either, without the server and
//! a catalog depending on each other.

use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;
use spoolframe_types::{ArtworkId, Item, Library, LibraryId, MediaId};

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

    /// Items whose titles match `query`, at most `limit` of them.
    async fn search(&self, query: &str, limit: u32) -> Result<Vec<Item>, CatalogError>;

    /// The local path of the item's media file: what the catalog reports,
    /// mapped to this host, and checked to be a readable file.
    async fn media_path(&self, id: &MediaId) -> Result<PathBuf, CatalogError>;

    /// The most recently added items, newest first, at most `limit`.
    async fn recently_added(&self, limit: u32) -> Result<Vec<Item>, CatalogError>;

    /// The image an `ArtworkId` names, at the size the catalog has it.
    /// Never anything but an image.
    async fn artwork(&self, id: &ArtworkId) -> Result<Image, CatalogError>;
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
