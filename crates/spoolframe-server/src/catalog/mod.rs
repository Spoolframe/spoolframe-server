//! The catalog the server serves from, as it is now (P3-10): none until
//! the server is set up, and replaced whole when its Plex servers or
//! libraries change (P3-11), without a restart. Everything that reads the
//! catalog holds this, so a change reaches the client protocol, the
//! artwork cache, watch state and the admin API at once.

pub mod composite;
pub mod ids;
pub mod replies;

use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use spoolframe_catalog::{
    Browsing, Catalog, CatalogError, Facts, Found, Image, ItemPage, ItemQuery, ItemWithChildren,
    Listed, Page, PreviewFrames, Reach, SearchQuery,
};
use spoolframe_plex::PlexCatalog;
use spoolframe_types::{
    ArtworkId, Item, ItemFilterValues, Library, LibraryId, MediaId, MediaKind, Namespace, PersonId,
};

pub use composite::{Composite, PlexSource, Served, Synced};
pub use replies::{Invalidated, Replies};

use replies::{Call, Reply};

/// The libraries now, if there are any. Every call without them is
/// [`CatalogError::NotSetUp`].
pub struct LiveCatalog {
    now: RwLock<Option<Arc<Composite>>>,
    /// The paired servers' cache (P8-02), given to each composite served.
    peers: OnceLock<Arc<dyn Catalog>>,
    /// Bumped at each [`LiveCatalog::replace`]: libraries may have gone
    /// (B-10).
    replaced: tokio::sync::watch::Sender<u64>,
    /// Shown each composite before it is served.
    hook: OnceLock<Hook>,
    /// Replies kept for a short while; forgotten at each replace.
    replies: Replies,
}

type Hook = Box<dyn Fn(&Composite) + Send + Sync>;

impl std::fmt::Debug for LiveCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveCatalog")
            .field("now", &self.now)
            .field("replies", &self.replies)
            .finish_non_exhaustive()
    }
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
            peers: OnceLock::new(),
            replaced: tokio::sync::watch::Sender::new(0),
            hook: OnceLock::new(),
            replies: Replies::new(crate::config::CatalogConfig::DEFAULT_MEMORY_MB << 20),
        }
    }

    /// The replies kept: their memory, their limit, their counts.
    pub fn replies(&self) -> &Replies {
        &self.replies
    }

    /// Show `hook` the composite now, and each one that replaces it before
    /// it is served: how the metrics see every Plex request as it ends.
    /// Only the first hook is kept.
    pub fn on_replace(&self, hook: impl Fn(&Composite) + Send + Sync + 'static) {
        if self.hook.set(Box::new(hook)).is_err() {
            return;
        }
        // After the hook is in place: a composite replacing this one now
        // is shown it by `replace`, and at worst twice.
        if let (Some(now), Some(hook)) = (self.composite(), self.hook.get()) {
            hook(&now);
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

    /// Serve `fed:` ids from `peers`, the paired servers' cache (P8-02),
    /// in the composite now and each one that replaces it. Only the first
    /// is kept.
    pub fn set_peers(&self, peers: Arc<dyn Catalog>) {
        if self.peers.set(peers).is_err() {
            return;
        }
        let now = self.composite();
        let mut held = self.now.write();
        // Unless it was replaced meanwhile, by one that has them already.
        if let Some(now) = now
            && held.as_ref().is_some_and(|h| Arc::ptr_eq(h, &now))
        {
            *held = Some(self.with_peers(now));
        }
    }

    /// `composite` with the paired servers' cache, if there is one.
    fn with_peers(&self, composite: Arc<Composite>) -> Arc<Composite> {
        match self.peers.get() {
            Some(peers) if !composite.has_peers() => {
                Arc::new(Arc::unwrap_or_clone(composite).with_peers(peers.clone()))
            }
            _ => composite,
        }
    }

    /// Serve from `now` from now on; `None` is not set up.
    pub fn replace(&self, now: Option<Arc<Composite>>) {
        let now = now.map(|c| self.with_peers(c));
        if let (Some(now), Some(hook)) = (&now, self.hook.get()) {
            hook(now);
        }
        *self.now.write() = now;
        self.replies.forget();
        self.replaced.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn current(&self) -> Result<Arc<Composite>, CatalogError> {
        self.composite().ok_or(CatalogError::NotSetUp)
    }
}

/// Replies are kept for Plex's answers only ([`Replies::read`] declines a
/// paired server's `fed:` call).
#[async_trait]
impl Catalog for LiveCatalog {
    async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
        self.replies
            .read(
                Call::Libraries,
                |r| match r {
                    Reply::Libraries(l) => Some(l),
                    _ => None,
                },
                Reply::Libraries,
                async { self.current()?.libraries().await },
            )
            .await
    }

    async fn items(&self, library: &LibraryId, page: Page) -> Result<ItemPage, CatalogError> {
        self.replies
            .read(
                Call::Items(library.to_string(), page.offset, page.limit),
                |r| match r {
                    Reply::Page(p) => Some(p),
                    _ => None,
                },
                Reply::Page,
                async { self.current()?.items(library, page).await },
            )
            .await
    }

    /// Sorted and filtered listings are not kept: a walk by watch state
    /// keeps its own, and each is as live as Plex.
    /// Not kept: a paired server's sync walks a library at its own pace.
    async fn items_with_facts(
        &self,
        library: &LibraryId,
        page: Page,
    ) -> Result<(ItemPage, Vec<Facts>), CatalogError> {
        self.current()?.items_with_facts(library, page).await
    }

    async fn details(&self, id: &MediaId) -> Result<(Vec<String>, Facts), CatalogError> {
        self.current()?.details(id).await
    }

    async fn browsing(&self, library: &Library) -> Result<Browsing, CatalogError> {
        self.current()?.browsing(library).await
    }

    async fn filter_values(&self, library: &Library) -> Result<ItemFilterValues, CatalogError> {
        self.current()?.filter_values(library).await
    }

    async fn query_items(
        &self,
        library: &LibraryId,
        query: &ItemQuery,
        page: Page,
    ) -> Result<ItemPage, CatalogError> {
        self.current()?.query_items(library, query, page).await
    }

    async fn walk_items(
        &self,
        library: &LibraryId,
        query: &ItemQuery,
    ) -> Result<Vec<Listed>, CatalogError> {
        self.current()?.walk_items(library, query).await
    }

    async fn items_by_id(&self, ids: &[MediaId]) -> Result<Vec<Item>, CatalogError> {
        self.current()?.items_by_id(ids).await
    }

    async fn item(&self, id: &MediaId) -> Result<ItemWithChildren, CatalogError> {
        self.replies
            .read(
                Call::Item(id.to_string()),
                |r| match r {
                    Reply::Item(i) => Some(i.as_ref()),
                    _ => None,
                },
                |i| Reply::Item(Box::new(i)),
                async { self.current()?.item(id).await },
            )
            .await
    }

    async fn episodes(&self, show: &MediaId) -> Result<Vec<Item>, CatalogError> {
        self.replies
            .read(Call::Episodes(show.to_string()), list, Reply::List, async {
                self.current()?.episodes(show).await
            })
            .await
    }

    /// Not kept here: All (PX-43) keeps its own list a while, as one
    /// sorted list rather than each library's reply.
    async fn tracks(&self, library: &LibraryId) -> Result<Vec<Item>, CatalogError> {
        self.current()?.tracks(library).await
    }

    async fn external_ids(&self, id: &MediaId) -> Result<Vec<String>, CatalogError> {
        self.replies
            .read(
                Call::ExternalIds(id.to_string()),
                |r| match r {
                    Reply::Ids(i) => Some(i),
                    _ => None,
                },
                Reply::Ids,
                async { self.current()?.external_ids(id).await },
            )
            .await
    }

    async fn find_external(
        &self,
        kind: MediaKind,
        external_ids: &[String],
    ) -> Result<Option<MediaId>, CatalogError> {
        self.current()?.find_external(kind, external_ids).await
    }

    async fn library_of(&self, id: &MediaId) -> Result<Option<LibraryId>, CatalogError> {
        self.current()?.library_of(id).await
    }

    async fn item_kept(&self, id: &MediaId) -> Result<(ItemWithChildren, Reach), CatalogError> {
        // The composite says where a paired server's item is reached.
        if id.namespace() == Namespace::Federated {
            return self.current()?.item_kept(id).await;
        }
        Ok((self.item(id).await?, Reach::Here))
    }

    async fn search(&self, query: &SearchQuery) -> Result<Found, CatalogError> {
        self.current()?.search(query).await
    }

    async fn person_items(&self, id: &PersonId) -> Result<Vec<Item>, CatalogError> {
        self.replies
            .read(
                Call::PersonItems(id.to_string()),
                list,
                Reply::List,
                async { self.current()?.person_items(id).await },
            )
            .await
    }

    async fn media_path(&self, id: &MediaId) -> Result<PathBuf, CatalogError> {
        self.current()?.media_path(id).await
    }

    async fn recently_added(&self, limit: u32) -> Result<Vec<Item>, CatalogError> {
        self.replies
            .read(Call::RecentlyAdded(limit), list, Reply::List, async {
                self.current()?.recently_added(limit).await
            })
            .await
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

/// A list of items in `reply`, if it is one.
fn list(reply: &Reply) -> Option<&Vec<Item>> {
    match reply {
        Reply::List(items) => Some(items),
        _ => None,
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

    #[test]
    fn the_hook_is_shown_the_composite_now_and_each_replacing_it() {
        let single = || {
            let plex = PlexCatalog::new("http://127.0.0.1:9".parse().unwrap(), None).unwrap();
            Some(Arc::new(Composite::single("Tower", plex)))
        };
        let live = LiveCatalog::new(single());
        let shown = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = shown.clone();
        live.on_replace(move |_| {
            count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        live.replace(None);
        live.replace(single());
        assert_eq!(shown.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    /// P8-02: the paired servers' cache serves `fed:` ids in the
    /// composite now and in each one that replaces it; every other id
    /// still goes to Plex.
    #[tokio::test]
    async fn fed_ids_go_to_the_cache_through_every_composite() {
        let single = || {
            let plex = PlexCatalog::new("http://127.0.0.1:9".parse().unwrap(), None).unwrap();
            Some(Arc::new(Composite::single("Tower", plex)))
        };
        let live = LiveCatalog::new(single());
        let fed: MediaId = "fed:movie:mzxw6ytbojtg633c:plex:1".parse().unwrap();
        assert!(matches!(
            live.item(&fed).await,
            Err(CatalogError::NotServed(_))
        ));
        let cache = crate::test_support::FakeCatalog::new("/nowhere/movie.mkv".into());
        live.set_peers(Arc::new(cache));
        // The fake says it has no such key: it was asked.
        assert!(matches!(
            live.item(&fed).await,
            Err(CatalogError::NotFound(_))
        ));
        live.replace(single());
        assert!(matches!(
            live.item(&fed).await,
            Err(CatalogError::NotFound(_))
        ));
        let plex: MediaId = "plex:movie:1".parse().unwrap();
        assert!(matches!(
            live.item(&plex).await,
            Err(CatalogError::Unreachable { .. })
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
