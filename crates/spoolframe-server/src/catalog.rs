//! The catalog the server serves from, as it is now (P3-10): none until
//! the server is set up, and replaced whole when its Plex settings change,
//! without a restart. Everything that reads the catalog holds this, so a
//! change reaches the client protocol, the artwork cache, watch state and
//! the admin API at once.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use spoolframe_catalog::{Catalog, CatalogError, Image, ItemPage, ItemWithChildren, Page};
use spoolframe_plex::PlexCatalog;
use spoolframe_types::{ArtworkId, Item, Library, LibraryId, MediaId};

/// The Plex catalog now, if there is one. Every call without one is
/// [`CatalogError::NotSetUp`].
#[derive(Debug, Default)]
pub struct LiveCatalog {
    plex: RwLock<Option<Arc<PlexCatalog>>>,
}

impl LiveCatalog {
    pub fn new(plex: Option<Arc<PlexCatalog>>) -> Self {
        Self {
            plex: RwLock::new(plex),
        }
    }

    /// The Plex catalog now. A call in progress keeps the one it began
    /// with.
    pub fn plex(&self) -> Option<Arc<PlexCatalog>> {
        self.plex.read().expect("never held across a panic").clone()
    }

    /// Serve from `plex` from now on; `None` is not set up.
    pub fn replace(&self, plex: Option<Arc<PlexCatalog>>) {
        *self.plex.write().expect("never held across a panic") = plex;
    }

    fn current(&self) -> Result<Arc<PlexCatalog>, CatalogError> {
        self.plex().ok_or(CatalogError::NotSetUp)
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

    async fn search(&self, query: &str, limit: u32) -> Result<Vec<Item>, CatalogError> {
        self.current()?.search(query, limit).await
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
        live.replace(Some(Arc::new(plex)));
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
