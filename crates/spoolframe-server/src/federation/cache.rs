//! What paired servers share, cached here (P8-02), as a [`Catalog`] of
//! `fed:` ids: one more backend of the composite
//! ([`crate::catalog::Composite::with_peers`]).
//!
//! Browsing reads this server's database only, so it works with the
//! origin offline. Artwork is files under the state directory: posters
//! fetched a few at a time after a sync, the rest when first asked for,
//! over the connection the replica holds to the origin ([`Links`]);
//! without one, an image not fetched yet is unavailable. Each origin's
//! files take at most the cache's limit, and all origins' together at
//! most the total, the least recently used going first. Only a JPEG, PNG
//! or WebP is kept. A cached image grants no access on its own: it is
//! served only while an available item names it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use spoolframe_catalog::{
    Catalog, CatalogError, Found, FoundGroup, Image, ItemPage, ItemWithChildren, Page, Reach,
    SearchQuery, episode_ids,
};
use spoolframe_federation::{DialError, Dialed};
use spoolframe_protocol::federation::{RemoteItem, RemoteKind};
use spoolframe_types::{
    ArtworkId, Item, Library, LibraryId, LibraryKind, MediaId, MediaKind, PersonId, SearchKind,
};

use super::items::{self, local};
use crate::store::federation::PeerRecord;
use crate::store::federation_cache::{BrowseLimits, Browsed, CachedFile, FileLimits};
use crate::store::{Store, StoreError};

/// The default limit on each origin's artwork files: 2 GiB.
pub const DEFAULT_MAX_BYTES: u64 = 2 << 30;

/// The default limit on every origin's artwork files together: 8 GiB.
pub const DEFAULT_TOTAL_BYTES: u64 = 8 << 30;

/// The connections held to the paired servers this one replicates from,
/// by origin (their server ids): what the replica dials, the cache fetches
/// artwork over, and what says whether an origin is online.
#[derive(Debug)]
pub struct Links {
    open: parking_lot::Mutex<HashMap<String, Arc<Dialed>>>,
    /// Told when an origin comes online or goes offline (P8-02's
    /// `LibrariesChanged`).
    changed: tokio::sync::broadcast::Sender<()>,
}

impl Default for Links {
    fn default() -> Self {
        Self {
            open: parking_lot::Mutex::default(),
            changed: tokio::sync::broadcast::Sender::new(16),
        }
    }
}

impl Links {
    /// Told each time an origin comes online or goes offline.
    pub fn changes(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.changed.subscribe()
    }

    /// The libraries' servers changed: nobody listening is fine.
    pub(crate) fn told(&self) {
        let _ = self.changed.send(());
    }

    /// The open connection to `peer`, the live pair as the caller read
    /// it, if there is one to the key it is pinned to (C8-01): nothing is
    /// sent to, or taken from, a key the pair has moved from. One that
    /// closed is forgotten; one to another key is left to the cleanup of
    /// the pair's move, as the caller's `peer` may be the older.
    pub fn pinned(&self, peer: &PeerRecord) -> Option<Arc<Dialed>> {
        self.open(&peer.server_id)
            .filter(|dialed| peer.pinned_to(&dialed.connection().remote_id()))
    }

    /// The connection to `origin` that is open now, whatever key it is
    /// to; one that closed is forgotten.
    fn open(&self, origin: &str) -> Option<Arc<Dialed>> {
        let mut open = self.open.lock();
        let dialed = open.get(origin)?;
        if dialed.connection().close_reason().is_some() {
            open.remove(origin);
            return None;
        }
        Some(dialed.clone())
    }

    /// `origin` is online over `dialed`, until that connection closes.
    pub(crate) fn set(&self, origin: &str, dialed: Arc<Dialed>) {
        let connection = dialed.connection().clone();
        if let Some(old) = self.open.lock().insert(origin.to_owned(), dialed) {
            close(&old);
        }
        self.told();
        let changed = self.changed.clone();
        tokio::spawn(async move {
            connection.closed().await;
            let _ = changed.send(());
        });
    }

    /// Close and forget the connection to `origin`.
    pub(crate) fn drop_origin(&self, origin: &str) {
        if let Some(old) = self.open.lock().remove(origin) {
            close(&old);
        }
    }

    /// Close `dialed`, and forget it as `origin`'s if it still is.
    pub(crate) fn forget(&self, origin: &str, dialed: &Arc<Dialed>) {
        super::forget_kept(&mut self.open.lock(), origin, dialed, |kept| kept);
        close(dialed);
    }

    /// Whether `origin` is online: a connection to it is open. Only what
    /// the pages say: anything sent goes over [`Links::pinned`].
    pub fn online(&self, origin: &str) -> bool {
        self.open(origin).is_some()
    }
}

fn close(dialed: &Dialed) {
    dialed
        .connection()
        .close(iroh::endpoint::VarInt::from_u32(0), b"done");
}

/// The paired servers' cache, as a catalog of `fed:` ids.
#[derive(Debug)]
pub struct PeerCache {
    store: Store,
    /// Where the artwork files go: a directory per origin.
    dir: PathBuf,
    /// [`FileLimits::per_origin`] and [`FileLimits::total`]: live
    /// settings.
    max_bytes: AtomicU64,
    total_bytes: AtomicU64,
    links: Arc<Links>,
    /// Each origin's posters that failed since its last sync: not asked
    /// for again until the next.
    misses: parking_lot::Mutex<HashMap<String, HashSet<String>>>,
    /// The most of a hierarchy browsed at once; the default but in tests.
    browse: BrowseLimits,
}

/// `browsed`, the items under `what` of `origin`'s: a hierarchy past what
/// is browsed at once is refused as too large, nothing of it read.
fn within_browse(what: &str, browsed: Browsed) -> Result<Vec<RemoteItem>, CatalogError> {
    match browsed {
        Browsed::Items(items) => Ok(items),
        Browsed::OverLimit { items, bytes } => Err(CatalogError::TooLarge {
            what: what.to_owned(),
            items,
            bytes,
        }),
    }
}

/// A store failure as a catalog's: the federation cache is the one
/// catalog that reads this server's store.
impl From<StoreError> for CatalogError {
    fn from(e: StoreError) -> Self {
        CatalogError::Backend {
            url: "the federation cache".into(),
            call: "a read".into(),
            item: None,
            message: e.to_string(),
        }
    }
}

fn foreign(id: &impl ToString) -> CatalogError {
    CatalogError::ForeignId(id.to_string())
}

/// The kind of item a library of `kind` lists at its top.
fn top_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "movies" => "movie",
        "shows" => "show",
        "music" => "artist",
        _ => return None,
    })
}

fn library_kind(kind: &str) -> Option<LibraryKind> {
    Some(match kind {
        "movies" => LibraryKind::Movies,
        "shows" => LibraryKind::Shows,
        "music" => LibraryKind::Music,
        _ => return None,
    })
}

/// The kind a search group finds, as the cache's tables name it; `None`
/// for people, who are not cached.
fn search_kind(kind: SearchKind) -> Option<&'static str> {
    Some(match kind {
        SearchKind::Movies => "movie",
        SearchKind::Shows => "show",
        SearchKind::Episodes => "episode",
        SearchKind::Artists => "artist",
        SearchKind::Albums => "album",
        SearchKind::Tracks => "track",
        SearchKind::People => return None,
    })
}

fn locals(origin: &str, items: &[RemoteItem]) -> Vec<Item> {
    items.iter().filter_map(|i| local(origin, i)).collect()
}

/// The file an origin's image is kept as: its id's hash, so any id makes
/// a plain file name.
fn file_name(artwork: &str) -> String {
    blake3::hash(artwork.as_bytes()).to_hex().to_string()
}

impl PeerCache {
    /// The cache over `store`, its artwork files under `dir` within
    /// `limits`, fetched over `links`.
    pub fn new(store: Store, dir: PathBuf, limits: FileLimits, links: Arc<Links>) -> Self {
        Self {
            store,
            dir,
            max_bytes: AtomicU64::new(limits.per_origin),
            total_bytes: AtomicU64::new(limits.total),
            links,
            misses: parking_lot::Mutex::default(),
            browse: BrowseLimits::default(),
        }
    }

    /// With `limits` on a hierarchy browsed at once instead of
    /// [`BrowseLimits::default`]: for tests, which set them small.
    #[must_use]
    pub fn with_browse_limits(mut self, limits: BrowseLimits) -> Self {
        self.browse = limits;
        self
    }

    /// At most `limit` of `origin`'s posters to fetch: an available
    /// item's, with no file yet, and not failed since the last sync.
    pub(crate) async fn missing_posters(
        &self,
        origin: &str,
        limit: u32,
    ) -> Result<Vec<String>, StoreError> {
        let missed = self.misses.lock().get(origin).map_or(0, HashSet::len);
        let posters = self
            .store
            .missing_posters(origin, limit.saturating_add(missed as u32))
            .await?;
        let misses = self.misses.lock();
        let missed = misses.get(origin);
        Ok(posters
            .into_iter()
            .filter(|p| missed.is_none_or(|m| !m.contains(p)))
            .take(limit as usize)
            .collect())
    }

    /// `origin`'s `poster` could not be fetched.
    pub(crate) fn missed(&self, origin: &str, poster: String) {
        self.misses
            .lock()
            .entry(origin.to_owned())
            .or_default()
            .insert(poster);
    }

    /// `origin` syncs: its posters that failed are asked for again.
    pub(crate) fn sync_began(&self, origin: &str) {
        self.misses.lock().remove(origin);
    }

    /// New limits on the artwork files: a live setting. Files past them
    /// go at the replica's next tick (`PeerCache::evict`), or as the
    /// next one is kept.
    pub fn set_limits(&self, limits: FileLimits) {
        self.max_bytes.store(limits.per_origin, Ordering::Relaxed);
        self.total_bytes.store(limits.total, Ordering::Relaxed);
    }

    fn limits(&self) -> FileLimits {
        FileLimits {
            per_origin: self.max_bytes.load(Ordering::Relaxed),
            total: self.total_bytes.load(Ordering::Relaxed),
        }
    }

    /// The files past the limits go (M5): a lower limit takes effect
    /// without waiting for the next image.
    pub(crate) async fn evict(&self) {
        match self.store.evict_cached_files(self.limits()).await {
            Ok(gone) => self.delete_gone(&gone).await,
            Err(e) => tracing::warn!(error = %e, "cannot hold the cached images to their limits"),
        }
    }

    /// Delete the files `gone` names, each of its origin.
    async fn delete_gone(&self, gone: &[(String, String)]) {
        for (origin, file) in gone {
            self.delete_files(origin, std::slice::from_ref(file)).await;
        }
    }

    fn origin_dir(&self, origin: &str) -> PathBuf {
        self.dir.join(origin)
    }

    /// Delete `files` of `origin`'s; one already gone is no matter.
    pub(crate) async fn delete_files(&self, origin: &str, files: &[String]) {
        let dir = self.origin_dir(origin);
        for file in files {
            if let Err(e) = tokio::fs::remove_file(dir.join(file)).await
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(origin, error = %e, "cannot delete a cached image");
            }
        }
    }

    /// Delete every file of `origin`'s, and its directory.
    pub(crate) async fn delete_origin(&self, origin: &str) {
        let dir = self.origin_dir(origin);
        if let Err(e) = tokio::fs::remove_dir_all(&dir).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(origin, error = %e, "cannot delete a paired server's cached images");
        }
    }

    /// Image `artwork` of `origin`'s from its file, if it is kept and
    /// readable.
    async fn read_file(&self, origin: &str, artwork: &str) -> Option<Image> {
        let kept = self
            .store
            .cached_file(origin, artwork)
            .await
            .inspect_err(|e| tracing::warn!(origin, error = %e, "cannot look a cached image up"))
            .ok()??;
        match tokio::fs::read(self.origin_dir(origin).join(&kept.file)).await {
            Ok(bytes) => Some(Image {
                bytes,
                content_type: kept.content_type,
            }),
            Err(e) => {
                tracing::warn!(origin, error = %e, "a cached image's file cannot be read");
                None
            }
        }
    }

    /// Fetch image `artwork` from `origin` over `dialed`, and keep it as a
    /// file; files past the limits go. What is not an image, by its type
    /// and its first bytes, is neither kept nor served (L7): `NotFound`.
    pub(crate) async fn fetch(
        &self,
        origin: &str,
        dialed: &Dialed,
        artwork: &str,
    ) -> Result<Image, DialError> {
        let (content_type, bytes) = dialed.artwork(artwork).await?;
        if !content_type.starts_with("image/") || !crate::artwork::is_raster(&bytes) {
            tracing::warn!(origin, content_type, "a paired server's image is not one");
            return Err(DialError::NotFound);
        }
        if let Err(e) = self.keep_file(origin, artwork, &content_type, &bytes).await {
            tracing::warn!(origin, error = %e, "cannot keep a paired server's image");
        }
        Ok(Image {
            bytes,
            content_type,
        })
    }

    async fn keep_file(
        &self,
        origin: &str,
        artwork: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        // One past a limit on its own would only go again at once.
        let limits = self.limits();
        if bytes.len() as u64 > limits.per_origin.min(limits.total) {
            return Ok(());
        }
        let dir = self.origin_dir(origin);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| e.to_string())?;
        let file = file_name(artwork);
        write_atomically(&dir, &file, bytes)
            .await
            .map_err(|e| e.to_string())?;
        let gone = self
            .store
            .keep_cached_file(
                origin,
                artwork,
                &CachedFile {
                    file,
                    content_type: content_type.to_owned(),
                },
                bytes.len() as u64,
                limits,
            )
            .await
            .map_err(|e| e.to_string())?;
        self.delete_gone(&gone).await;
        Ok(())
    }

    /// Cached item `id`, available: `NotServed` once its library or pair
    /// went, `NotFound` when nothing is cached under it.
    async fn available(&self, id: &MediaId) -> Result<(String, RemoteItem, String), CatalogError> {
        let (origin, remote) = id.remote().ok_or_else(|| foreign(id))?;
        let cached = self
            .store
            .cached_item(origin, &remote.to_string())
            .await?
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))?;
        if cached.unavailable {
            return Err(CatalogError::NotServed(id.to_string()));
        }
        if items::kind(cached.item.kind) != Some(id.kind()) {
            return Err(CatalogError::NotFound(id.to_string()));
        }
        Ok((origin.to_owned(), cached.item, cached.library))
    }

    /// The title `remote` of `origin`'s is a copy of ([`Found::titles`]):
    /// its external ids, and an episode's show's with its season and
    /// number. A show not cached adds none.
    async fn title(&self, origin: &str, remote: &RemoteItem) -> Result<Vec<String>, StoreError> {
        let mut ids = remote.external_ids.clone();
        if let (RemoteKind::Episode, Some(show), Some(season), Some(episode)) = (
            remote.kind,
            &remote.grandparent,
            remote.parent_index,
            remote.index,
        ) && let Some(show) = self.store.cached_item(origin, show).await?
        {
            ids.extend(episode_ids(&show.item.external_ids, season, episode));
        }
        Ok(ids)
    }

    /// Whether `origin` is online: its items play now (P8-03).
    pub fn online(&self, origin: &str) -> bool {
        self.links.online(origin)
    }

    /// What `origin` shared most recently, as [`Catalog::recently_added`]
    /// gives it for every origin: its own Home row (P8-02).
    pub async fn recently_added_from(
        &self,
        origin: &str,
        limit: u32,
    ) -> Result<Vec<Item>, StoreError> {
        let recent = self.store.recent_cached(Some(origin), limit).await?;
        Ok(recent
            .iter()
            .filter_map(|(origin, item)| local(origin, item))
            .collect())
    }
}

/// Write `bytes` as `file` in `dir`, whole or not at all.
async fn write_atomically(dir: &Path, file: &str, bytes: &[u8]) -> std::io::Result<()> {
    let part = dir.join(format!("{file}.part"));
    tokio::fs::write(&part, bytes).await?;
    tokio::fs::rename(&part, dir.join(file)).await
}

#[async_trait]
impl Catalog for PeerCache {
    async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
        let cached = self.store.cached_libraries(None).await?;
        Ok(cached
            .into_iter()
            .filter_map(|l| {
                Some(Library {
                    id: LibraryId::federated(&l.origin, &l.library.parse().ok()?).ok()?,
                    title: l.title,
                    kind: library_kind(&l.kind)?,
                    server: None,
                })
            })
            .collect())
    }

    async fn items(&self, library: &LibraryId, page: Page) -> Result<ItemPage, CatalogError> {
        let (origin, remote) = library.remote().ok_or_else(|| foreign(library))?;
        let cached = self
            .store
            .cached_library(origin, &remote.to_string())
            .await?
            .ok_or_else(|| CatalogError::NotFound(library.to_string()))?;
        let kind =
            top_kind(&cached.kind).ok_or_else(|| CatalogError::NotFound(library.to_string()))?;
        let (items, total) = self
            .store
            .cached_page(origin, &cached.library, kind, page.offset, page.limit)
            .await?;
        Ok(ItemPage {
            items: locals(origin, &items),
            total,
        })
    }

    async fn item(&self, id: &MediaId) -> Result<ItemWithChildren, CatalogError> {
        let (origin, remote, _) = self.available(id).await?;
        let item = local(&origin, &remote).ok_or_else(|| CatalogError::NotFound(id.to_string()))?;
        let children = self
            .store
            .cached_children(&origin, &remote.id, self.browse)
            .await?;
        let children = within_browse(&remote.id, children)?;
        Ok(ItemWithChildren {
            item,
            children: locals(&origin, &children),
            external_ids: remote.external_ids,
            markers: Vec::new(),
        })
    }

    async fn episodes(&self, show: &MediaId) -> Result<Vec<Item>, CatalogError> {
        let (origin, remote, _) = self.available(show).await?;
        let episodes = self
            .store
            .cached_grandchildren(&origin, &remote.id, "episode", self.browse)
            .await?;
        let episodes = within_browse(&remote.id, episodes)?;
        Ok(locals(&origin, &episodes))
    }

    async fn tracks(&self, library: &LibraryId) -> Result<Vec<Item>, CatalogError> {
        let (origin, remote) = library.remote().ok_or_else(|| foreign(library))?;
        let remote = remote.to_string();
        let tracks = self
            .store
            .cached_all(origin, &remote, "track", self.browse)
            .await?;
        let tracks = within_browse(&remote, tracks)?;
        Ok(locals(origin, &tracks))
    }

    async fn external_ids(&self, id: &MediaId) -> Result<Vec<String>, CatalogError> {
        Ok(self.available(id).await?.1.external_ids)
    }

    async fn find_external(
        &self,
        kind: MediaKind,
        external_ids: &[String],
    ) -> Result<Option<MediaId>, CatalogError> {
        let found = self
            .store
            .find_cached_external(kind.as_str(), external_ids)
            .await?;
        Ok(found.and_then(|(origin, item)| items::media_id(&origin, kind, &item)))
    }

    async fn item_kept(&self, id: &MediaId) -> Result<(ItemWithChildren, Reach), CatalogError> {
        let (origin, remote) = id.remote().ok_or_else(|| foreign(id))?;
        let cached = self
            .store
            .cached_item(origin, &remote.to_string())
            .await?
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))?;
        if !cached.unavailable {
            let reach = if self.online(origin) {
                Reach::Here
            } else {
                Reach::Offline
            };
            return Ok((self.item(id).await?, reach));
        }
        let item = local(origin, &cached.item)
            .filter(|item| item.id().kind() == id.kind())
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))?;
        let kept = ItemWithChildren {
            item,
            children: Vec::new(),
            external_ids: cached.item.external_ids,
            markers: Vec::new(),
        };
        Ok((kept, Reach::Gone))
    }

    async fn library_of(&self, id: &MediaId) -> Result<Option<LibraryId>, CatalogError> {
        let (origin, _, library) = self.available(id).await?;
        Ok(library
            .parse()
            .ok()
            .and_then(|l| LibraryId::federated(&origin, &l).ok()))
    }

    /// Each kind asked for, by title, year and library, over the cache
    /// alone: no paired server is asked anything. A library scope that is
    /// not a paired server's finds nothing here, nor do people, whose
    /// items are not shared. Each hit's title is its replicated external
    /// ids, and an episode's show's with its season and number, as the
    /// watch state's title key has them: what the server's search merges
    /// a title found here and on this server by (P8-05).
    async fn search(&self, query: &SearchQuery) -> Result<Found, CatalogError> {
        let scope = match &query.library {
            Some(library) => match library.remote() {
                Some((origin, remote)) => Some((origin.to_owned(), remote.to_string())),
                None => {
                    return Ok(Found::of(
                        query.kinds.iter().map(|&k| FoundGroup::empty(k)).collect(),
                    ));
                }
            },
            None => None,
        };
        let mut groups = Vec::with_capacity(query.kinds.len());
        let mut titles = HashMap::new();
        for &kind in &query.kinds {
            let Some(name) = search_kind(kind) else {
                groups.push(FoundGroup::empty(kind));
                continue;
            };
            let (found, total) = self
                .store
                .search_cached(
                    name,
                    &query.text,
                    query.year,
                    scope.as_ref().map(|(o, l)| (o.as_str(), l.as_str())),
                    query.limit,
                )
                .await?;
            let mut items = Vec::with_capacity(found.len());
            for (origin, remote) in &found {
                let Some(item) = local(origin, remote) else {
                    continue;
                };
                titles.insert(item.id().clone(), self.title(origin, remote).await?);
                items.push(item);
            }
            groups.push(FoundGroup {
                kind,
                items,
                people: Vec::new(),
                total,
            });
        }
        Ok(Found { groups, titles })
    }

    /// People are the origin's: their items are not shared.
    async fn person_items(&self, id: &PersonId) -> Result<Vec<Item>, CatalogError> {
        Err(CatalogError::NotFound(id.to_string()))
    }

    /// A paired server's item has no file here: it plays from its origin
    /// (P8-04), never from this server's disk.
    async fn media_path(&self, id: &MediaId) -> Result<PathBuf, CatalogError> {
        Err(CatalogError::NotServed(id.to_string()))
    }

    /// The films, shows and albums cached most recently, of every paired
    /// server: when this server first cached them, not when their origin
    /// added them.
    async fn recently_added(&self, limit: u32) -> Result<Vec<Item>, CatalogError> {
        let recent = self.store.recent_cached(None, limit).await?;
        Ok(recent
            .iter()
            .filter_map(|(origin, item)| local(origin, item))
            .collect())
    }

    async fn authorize_artwork(&self, id: &ArtworkId) -> Result<(), CatalogError> {
        let (origin, remote) = id.remote().ok_or_else(|| foreign(id))?;
        let named = self
            .store
            .cached_artwork_named(origin, &remote.to_string())
            .await?;
        if named {
            Ok(())
        } else {
            Err(CatalogError::NotFound(id.to_string()))
        }
    }

    async fn artwork(&self, id: &ArtworkId) -> Result<Image, CatalogError> {
        self.authorize_artwork(id).await?;
        let (origin, remote) = id.remote().ok_or_else(|| foreign(id))?;
        let remote = remote.to_string();
        if let Some(image) = self.read_file(origin, &remote).await {
            return Ok(image);
        }
        let unreachable = |e: String| CatalogError::Unreachable {
            url: format!("paired server {origin}"),
            call: "artwork".into(),
            source: e.into(),
        };
        let peer = self
            .store
            .live_peer_by_server_id(origin)
            .await?
            .ok_or_else(|| unreachable("it is not paired".into()))?;
        let dialed = self
            .links
            .pinned(&peer)
            .ok_or_else(|| unreachable("it is offline".into()))?;
        match self.fetch(origin, &dialed, &remote).await {
            Ok(image) => Ok(image),
            Err(DialError::NotShared | DialError::NotFound) => {
                Err(CatalogError::NotFound(id.to_string()))
            }
            Err(e) => Err(unreachable(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use spoolframe_protocol::federation::{RemoteKind, RemoteLibraryKind, SharedLibrary};

    use super::*;
    use crate::store::federation::NewPeer;
    use crate::store::federation_cache::{CacheSynced, OriginLimits};
    use crate::store::tests::{TestStore, state_dir, test_store};

    const ORIGIN: &str = "aaaaaaaaaaaaaaaa";

    fn remote(
        id: &str,
        kind: RemoteKind,
        parent: Option<&str>,
        grandparent: Option<&str>,
        index: Option<u32>,
    ) -> RemoteItem {
        RemoteItem {
            id: id.into(),
            kind,
            title: id.into(),
            parent: parent.map(str::to_owned),
            grandparent: grandparent.map(str::to_owned),
            parent_title: None,
            grandparent_title: None,
            index,
            parent_index: index.map(|_| 1),
            year: None,
            date: None,
            // Padding, so each body is a few hundred bytes.
            summary: Some("s".repeat(200)),
            content_rating: None,
            duration_ms: None,
            count: None,
            genres: Vec::new(),
            artwork: Vec::new(),
            external_ids: Vec::new(),
            hash: String::new(),
        }
        .hashed()
    }

    /// A show of one season of four episodes in Shows (`plex:2`), and an
    /// artist of one album of four tracks in Music (`plex:3`), cached
    /// from the origin, whole, within the sync limits.
    async fn cached_hierarchies(test: &str) -> (TestStore, String) {
        let store = test_store(test).await;
        let peer = store
            .add_peer(
                NewPeer {
                    server_id: ORIGIN,
                    endpoint_id: &"0a".repeat(32),
                    display_name: "Alice's server",
                    addrs: &[],
                    relays: &[],
                },
                "bob",
            )
            .await
            .unwrap();
        let key = store
            .federation_peer(&peer.id)
            .await
            .unwrap()
            .unwrap()
            .endpoint_id;
        let show = remote("plex:show:1", RemoteKind::Show, None, None, None);
        let season = remote(
            "plex:season:2",
            RemoteKind::Season,
            Some(&show.id),
            None,
            Some(1),
        );
        let episodes = (10..14).map(|n| {
            remote(
                &format!("plex:episode:{n}"),
                RemoteKind::Episode,
                Some(&season.id),
                Some(&show.id),
                Some(n),
            )
        });
        let shows: Vec<RemoteItem> = [show.clone(), season.clone()]
            .into_iter()
            .chain(episodes)
            .collect();
        let artist = remote("plex:artist:5", RemoteKind::Artist, None, None, None);
        let album = remote(
            "plex:album:6",
            RemoteKind::Album,
            Some(&artist.id),
            None,
            None,
        );
        let tracks = (20..24).map(|n| {
            remote(
                &format!("plex:track:{n}"),
                RemoteKind::Track,
                Some(&album.id),
                Some(&artist.id),
                Some(n),
            )
        });
        let music: Vec<RemoteItem> = [artist.clone(), album.clone()]
            .into_iter()
            .chain(tracks)
            .collect();
        for (library, title, kind, name, items) in [
            ("plex:2", "Shows", RemoteLibraryKind::Shows, "shows", shows),
            ("plex:3", "Music", RemoteLibraryKind::Music, "music", music),
        ] {
            store
                .cache_library(
                    ORIGIN,
                    &peer.id,
                    &SharedLibrary {
                        id: library.into(),
                        title: title.into(),
                        kind,
                    },
                    name,
                )
                .await
                .unwrap();
            store.begin_cache_sync(ORIGIN, library).await.unwrap();
            store
                .stage_cache_page(ORIGIN, library, &items, &[])
                .await
                .unwrap();
            let synced = store
                .finish_cache_sync(
                    ORIGIN,
                    (&peer.id, &key),
                    library,
                    "c1",
                    true,
                    OriginLimits::NONE,
                )
                .await
                .unwrap();
            assert_eq!(synced, CacheSynced::Written);
        }
        (store, peer.id)
    }

    /// The cache over `store`, browsing at most `limits` at once.
    async fn cache(test: &str, store: &TestStore, limits: BrowseLimits) -> PeerCache {
        let dir = state_dir(&format!("{test}-art")).await;
        PeerCache::new(
            store.store.clone(),
            dir.as_ref().join("peers"),
            FileLimits {
                per_origin: u64::MAX,
                total: u64::MAX,
            },
            Arc::new(Links::default()),
        )
        .with_browse_limits(limits)
    }

    fn fed(id: &str) -> MediaId {
        MediaId::federated(ORIGIN, &id.parse().unwrap()).unwrap()
    }

    fn fed_library(id: &str) -> LibraryId {
        LibraryId::federated(ORIGIN, &id.parse().unwrap()).unwrap()
    }

    /// The refusal: too large, naming what was browsed.
    fn refused(what: &str, e: CatalogError) -> bool {
        matches!(e, CatalogError::TooLarge { what: item, .. } if item == what)
    }

    /// Y8-01: a season of more episodes than are browsed at once, or of
    /// more bytes of them, is refused as the client's `Item`, counted
    /// before a body is read; one within the limits is browsed whole.
    #[tokio::test]
    async fn a_parent_of_more_children_than_are_browsed_at_once_is_refused() {
        let (store, _) = cached_hierarchies("fed-browse-children").await;
        let four_items = BrowseLimits {
            items: 4,
            bytes: u64::MAX,
        };
        let season = fed("plex:season:2");
        let found = cache("fed-browse-children", &store, four_items)
            .await
            .item(&season)
            .await
            .unwrap();
        assert_eq!(found.children.len(), 4);
        for limits in [
            BrowseLimits {
                items: 3,
                bytes: u64::MAX,
            },
            // Four bodies of a few hundred bytes each.
            BrowseLimits {
                items: 4,
                bytes: 1000,
            },
        ] {
            let e = cache("fed-browse-children", &store, limits)
                .await
                .item(&season)
                .await
                .expect_err("past the limits");
            assert!(refused("plex:season:2", e));
        }
        // The show itself, of one season, is browsed under any of them.
        cache(
            "fed-browse-children",
            &store,
            BrowseLimits {
                items: 1,
                bytes: 1000,
            },
        )
        .await
        .item(&fed("plex:show:1"))
        .await
        .unwrap();
    }

    /// Y8-01: a show's episodes, every season's at once, are held to the
    /// same limits.
    #[tokio::test]
    async fn a_show_of_more_episodes_than_are_browsed_at_once_is_refused() {
        let (store, _) = cached_hierarchies("fed-browse-episodes").await;
        let show = fed("plex:show:1");
        let episodes = cache(
            "fed-browse-episodes",
            &store,
            BrowseLimits {
                items: 4,
                bytes: u64::MAX,
            },
        )
        .await
        .episodes(&show)
        .await
        .unwrap();
        assert_eq!(episodes.len(), 4);
        let e = cache(
            "fed-browse-episodes",
            &store,
            BrowseLimits {
                items: 3,
                bytes: u64::MAX,
            },
        )
        .await
        .episodes(&show)
        .await
        .expect_err("past the limits");
        assert!(refused("plex:show:1", e));
    }

    /// Y8-01: a music library's tracks, every album's at once (All,
    /// PX-43), are held to the same limits; its albums browse still.
    #[tokio::test]
    async fn a_library_of_more_tracks_than_are_browsed_at_once_is_refused() {
        let (store, _) = cached_hierarchies("fed-browse-tracks").await;
        let music = fed_library("plex:3");
        let tracks = cache(
            "fed-browse-tracks",
            &store,
            BrowseLimits {
                items: 4,
                bytes: u64::MAX,
            },
        )
        .await
        .tracks(&music)
        .await
        .unwrap();
        assert_eq!(tracks.len(), 4);
        let three = cache(
            "fed-browse-tracks",
            &store,
            BrowseLimits {
                items: 3,
                bytes: u64::MAX,
            },
        )
        .await;
        let e = three.tracks(&music).await.expect_err("past the limits");
        assert!(refused("plex:3", e));
        // The album, of four tracks, is past them too; the artist, of one
        // album, is not.
        assert!(refused(
            "plex:album:6",
            three
                .item(&fed("plex:album:6"))
                .await
                .expect_err("past the limits")
        ));
        three.item(&fed("plex:artist:5")).await.unwrap();
    }

    /// The review of Y8-01: a music library of 50,000 tracks, each body
    /// as a real track's measures and some, browses whole for All under
    /// the default limits.
    #[tokio::test]
    async fn a_library_of_fifty_thousand_tracks_browses_for_all() {
        let store = test_store("fed-browse-fifty-thousand").await;
        let peer = store
            .add_peer(
                NewPeer {
                    server_id: ORIGIN,
                    endpoint_id: &"0a".repeat(32),
                    display_name: "Alice's server",
                    addrs: &[],
                    relays: &[],
                },
                "bob",
            )
            .await
            .unwrap();
        let key = store
            .federation_peer(&peer.id)
            .await
            .unwrap()
            .unwrap()
            .endpoint_id;
        store
            .cache_library(
                ORIGIN,
                &peer.id,
                &SharedLibrary {
                    id: "plex:3".into(),
                    title: "Music".into(),
                    kind: RemoteLibraryKind::Music,
                },
                "music",
            )
            .await
            .unwrap();
        store.begin_cache_sync(ORIGIN, "plex:3").await.unwrap();
        let track = |n: u32| {
            RemoteItem {
                id: format!("plex:track:{n}"),
                kind: RemoteKind::Track,
                title: format!("Track {n}: a title about as long as a real one"),
                parent: Some(format!("plex:album:{}", n / 12)),
                grandparent: Some(format!("plex:artist:{}", n / 120)),
                parent_title: Some("An album title of the usual length".into()),
                grandparent_title: Some("An artist's name".into()),
                index: Some(n % 12),
                parent_index: Some(1),
                year: Some(2001),
                date: None,
                summary: None,
                content_rating: None,
                duration_ms: Some(240_000),
                count: None,
                genres: vec!["Electronic".into()],
                artwork: vec![spoolframe_protocol::federation::RemoteArtwork {
                    id: format!("plex:/library/metadata/{}/thumb/1700000000", n / 12),
                    kind: spoolframe_protocol::federation::RemoteArtworkKind::Poster,
                }],
                external_ids: vec![format!("mbid://a1b2c3d4-e5f6-7890-abcd-ef12345{n:05}")],
                hash: String::new(),
            }
            .hashed()
        };
        let body = serde_json::to_string(&track(49_999)).unwrap().len();
        assert!(body >= 600, "a track's body is {body} bytes");
        for page in 0..50 {
            let items: Vec<RemoteItem> = (page * 1000..(page + 1) * 1000).map(track).collect();
            store
                .stage_cache_page(ORIGIN, "plex:3", &items, &[])
                .await
                .unwrap();
        }
        let synced = store
            .finish_cache_sync(
                ORIGIN,
                (&peer.id, &key),
                "plex:3",
                "c1",
                true,
                OriginLimits::NONE,
            )
            .await
            .unwrap();
        assert_eq!(synced, CacheSynced::Written);
        let tracks = cache("fed-browse-fifty-thousand", &store, BrowseLimits::default())
            .await
            .tracks(&fed_library("plex:3"))
            .await
            .unwrap();
        assert_eq!(tracks.len(), 50_000);
    }
}
