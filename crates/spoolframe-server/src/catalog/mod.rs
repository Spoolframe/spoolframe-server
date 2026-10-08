//! The catalog the server serves from, as it is now (P3-10): none until
//! the server is set up, and replaced whole when its Plex servers or
//! libraries change (P3-11), without a restart. Everything that reads the
//! catalog holds this, so a change reaches the client protocol, the
//! artwork cache, watch state and the admin API at once.

pub mod composite;
pub mod ids;

use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use spoolframe_catalog::{
    Catalog, CatalogError, Found, Image, ItemPage, ItemWithChildren, Page, PreviewFrames,
    SearchQuery,
};
use spoolframe_plex::PlexCatalog;
use spoolframe_types::{ArtworkId, Item, Library, LibraryId, MediaId, PersonId};

pub use composite::{Composite, PlexSource, Served, Synced};

/// The libraries now, if there are any. Every call without them is
/// [`CatalogError::NotSetUp`].
#[derive(Debug)]
pub struct LiveCatalog {
    now: RwLock<Option<Arc<Composite>>>,
    /// Bumped at each [`LiveCatalog::replace`]: libraries may have gone
    /// (B-10).
    replaced: tokio::sync::watch::Sender<u64>,
}

impl Default for LiveCatalog {
    fn default() -> Self {
        Self::new(None)
    }
}

impl LiveCatalog {
    pub fn new(now: Option<Arc<Composite>>) -> Self {
        Self {
            now: RwLock::new(now),
            replaced: tokio::sync::watch::Sender::new(0),
        }
    }

    /// Changes each time the catalog is replaced: a library or Plex
    /// server was added, removed or changed, so an open playback's item
    /// may no longer be served.
    pub fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.replaced.subscribe()
    }

    /// The Plex servers and libraries now. A call in progress keeps the
    /// ones it began with.
    pub fn composite(&self) -> Option<Arc<Composite>> {
        self.now.read().clone()
    }

    /// The first Plex server's catalog now: the one PX-04's watch sync and
    /// the wizard's Plex step are about.
    pub fn plex(&self) -> Option<Arc<PlexCatalog>> {
        self.composite().map(|c| c.first().catalog.clone())
    }

    /// Serve from `now` from now on; `None` is not set up.
    pub fn replace(&self, now: Option<Arc<Composite>>) {
        *self.now.write() = now;
        self.replaced.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn current(&self) -> Result<Arc<Composite>, CatalogError> {
        self.composite().ok_or(CatalogError::NotSetUp)
    }
}

#[async_trait]
impl Catalog for LiveCatalog {
    async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
        self.current()?.libraries().await
    }

    async fn items(&self, library: &LibraryId, page: Page) -> Result<ItemPage, CatalogError> {
        self.current()?.items(library, page).await
    }

    async fn item(&self, id: &MediaId) -> Result<ItemWithChildren, CatalogError> {
        self.current()?.item(id).await
    }

    async fn external_ids(&self, id: &MediaId) -> Result<Vec<String>, CatalogError> {
        self.current()?.external_ids(id).await
    }

    async fn search(&self, query: &SearchQuery) -> Result<Found, CatalogError> {
        self.current()?.search(query).await
    }

    async fn person_items(&self, id: &PersonId) -> Result<Vec<Item>, CatalogError> {
        self.current()?.person_items(id).await
    }

    async fn media_path(&self, id: &MediaId) -> Result<PathBuf, CatalogError> {
        self.current()?.media_path(id).await
    }

    async fn recently_added(&self, limit: u32) -> Result<Vec<Item>, CatalogError> {
        self.current()?.recently_added(limit).await
    }

    async fn artwork(&self, id: &ArtworkId) -> Result<Image, CatalogError> {
        self.current()?.artwork(id).await
    }

    async fn authorize_artwork(&self, id: &ArtworkId) -> Result<(), CatalogError> {
        self.current()?.authorize_artwork(id).await
    }

    async fn preview_frames(&self, id: &MediaId) -> Result<Option<PreviewFrames>, CatalogError> {
        self.current()?.preview_frames(id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn without_plex_every_call_is_not_set_up() {
        let live = LiveCatalog::default();
        assert!(matches!(
            live.libraries().await,
            Err(CatalogError::NotSetUp)
        ));
        let id: MediaId = "plex:movie:1".parse().unwrap();
        assert!(matches!(
            live.media_path(&id).await,
            Err(CatalogError::NotSetUp)
        ));
    }

    #[tokio::test]
    async fn a_replaced_catalog_serves_the_next_call() {
        let live = LiveCatalog::default();
        let plex = PlexCatalog::new("http://127.0.0.1:9".parse().unwrap(), None).unwrap();
        live.replace(Some(Arc::new(Composite::single("Tower", plex))));
        // Plex is not there, but it is asked: the call is no longer
        // refused as not set up.
        assert!(matches!(
            live.libraries().await,
            Err(CatalogError::Unreachable { .. })
        ));
        live.replace(None);
        assert!(matches!(
            live.libraries().await,
            Err(CatalogError::NotSetUp)
        ));
    }
}
