//! The sharing server's side of replication (P8-02): what it answers a
//! pair's `Libraries`, `Sync` and `Artwork` with.
//!
//! There is no poller here, nor any change feed: Plex has none. A sync
//! walks the library through the catalog when the pair asks, and diffs it
//! against what this server last gave that pair ([`crate::store::federation_sync`]).
//! A library not shared with the pair, or no longer served, is refused as
//! not shared, and the pair drops its cache of it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use spoolframe_catalog::{CatalogError, Page};
use spoolframe_federation::{Shares, SyncAnswer, Unshared};
use spoolframe_protocol::ServerId;
use spoolframe_protocol::federation::{
    AssertUser, AssertionRefused, RemoteItem, RemoteLibraryKind, SharedLibrary, SyncLibrary,
    SyncMode, TokenGranted,
};
use spoolframe_types::{ArtworkId, Item, Library, LibraryId, LibraryKind};
use uuid::Uuid;

use super::Federation;
use super::items::remote;
use crate::auth::{Bucket, TokenBucket};
use crate::keyed_locks::KeyedLocks;
use crate::store::StoreError;
use crate::store::federation::PeerRecord;
use crate::store::federation_sync::{KeptIds, SentItem};

/// How many top-level items a walk asks the catalog for at once.
const WALK_PAGE: u32 = 200;

/// How long a library's walk answers every pair's sync (H2): a pair that
/// asks again within it, or another pair, is answered from the same walk,
/// so no pair makes this server walk a library more often.
pub const WALK_REUSE: Duration = Duration::from_secs(60);

/// Each pair's images: a first sync fetches every poster, so a burst,
/// then a hundred a second.
const ARTWORK_ASKS: Bucket = Bucket {
    capacity: 200,
    refill_every: Duration::from_millis(10),
};

/// Each pair's assertions: one per play its users start.
const ASSERTIONS: Bucket = Bucket {
    capacity: 30,
    refill_every: Duration::from_secs(1),
};

/// The external ids a walk knows: those kept, by item, with the hash of
/// the rest of it then; and those it read anew.
struct Ids {
    /// The library walked.
    library: String,
    kept: HashMap<String, (String, Vec<String>)>,
    read: Vec<KeptIds>,
}

/// A library's items, walked: shared by the pairs it answers.
type Walk = Arc<Vec<RemoteItem>>;

/// What paces the pairs' asks (H2): one walk of a library at a time, kept
/// for [`WALK_REUSE`]; one sync per pair and library at a time; and each
/// pair's images and assertions, by token bucket.
#[derive(Debug)]
pub(crate) struct Answering {
    /// Held while a library is walked, by library.
    walking: KeyedLocks,
    /// Each library's last walk, and when it was made.
    walks: parking_lot::Mutex<HashMap<String, (Instant, Walk)>>,
    reuse: parking_lot::Mutex<Duration>,
    /// Held while a pair's sync of a library is answered, by pair and
    /// library: its kept state is read and written by one at a time.
    syncing: KeyedLocks,
    /// Each pair's buckets, by its server id: images, then assertions.
    asks: parking_lot::Mutex<HashMap<String, Arc<(TokenBucket, TokenBucket)>>>,
}

impl Default for Answering {
    fn default() -> Self {
        Self {
            walking: KeyedLocks::default(),
            walks: parking_lot::Mutex::default(),
            reuse: parking_lot::Mutex::new(WALK_REUSE),
            syncing: KeyedLocks::default(),
            asks: parking_lot::Mutex::default(),
        }
    }
}

/// What a pair asks that is paced by bucket.
#[derive(Debug, Clone, Copy)]
enum Ask {
    Artwork,
    Assertion,
}

impl Answering {
    /// Whether pair `peer` may ask `ask` now.
    fn allowed(&self, peer: &str, ask: Ask) -> bool {
        let buckets = self
            .asks
            .lock()
            .entry(peer.to_owned())
            .or_insert_with(|| {
                Arc::new((TokenBucket::new(ARTWORK_ASKS), TokenBucket::new(ASSERTIONS)))
            })
            .clone();
        let bucket = match ask {
            Ask::Artwork => &buckets.0,
            Ask::Assertion => &buckets.1,
        };
        bucket.take(Instant::now()).is_ok()
    }

    /// The walk of `library` made within the reuse, if there is one.
    fn reused(&self, library: &str) -> Option<Walk> {
        let reuse = *self.reuse.lock();
        self.walks
            .lock()
            .get(library)
            .filter(|(at, _)| at.elapsed() < reuse)
            .map(|(_, walk)| walk.clone())
    }

    /// Let the walks past their reuse go: at each replica tick, so one of
    /// a library no pair asks for again is not held.
    pub(super) fn prune_walks(&self) {
        let reuse = *self.reuse.lock();
        self.walks.lock().retain(|_, (at, _)| at.elapsed() < reuse);
    }
}

fn library_kind(kind: LibraryKind) -> RemoteLibraryKind {
    match kind {
        LibraryKind::Movies => RemoteLibraryKind::Movies,
        LibraryKind::Shows => RemoteLibraryKind::Shows,
        LibraryKind::Music => RemoteLibraryKind::Music,
    }
}

fn unavailable(what: &str, e: impl std::fmt::Display) -> Unshared {
    tracing::warn!(error = %e, "cannot answer a paired server's {what}");
    Unshared::Unavailable
}

impl Federation {
    /// The live pair pinned under `server_id`: a connection's standing
    /// said so when it said hello, and it may have been unpaired since.
    async fn pair(&self, server_id: &ServerId) -> Result<PeerRecord, Unshared> {
        self.store()
            .live_peer_by_server_id(server_id.as_str())
            .await
            .map_err(|e| unavailable("request", e))?
            .ok_or(Unshared::NotShared)
    }

    /// Library `id`, if it is shared with `peer` and served now.
    async fn shared_library(&self, peer: &PeerRecord, id: &str) -> Result<Library, Unshared> {
        self.granted(&peer.id, id)
            .await
            .map_err(|e| unavailable("sync", e))?
            .ok_or(Unshared::NotShared)
    }

    /// How long a library's walk answers every pair's sync: a live
    /// setting, [`WALK_REUSE`] at first. Zero walks the library at each
    /// sync, as a test that changes it between two syncs needs.
    pub fn set_walk_reuse(&self, reuse: Duration) {
        *self.answering.reuse.lock() = reuse;
    }

    /// `library` walked, or its walk made within the reuse: one walk at a
    /// time per library, for every pair.
    async fn walked(&self, library: &LibraryId) -> Result<Walk, CatalogError> {
        let key = library.to_string();
        let _walking = self.answering.walking.lock(&key).await;
        if let Some(walk) = self.answering.reused(&key) {
            return Ok(walk);
        }
        let walk = Arc::new(self.walk(library).await?);
        self.answering
            .walks
            .lock()
            .insert(key, (Instant::now(), walk.clone()));
        Ok(walk)
    }

    /// Every item of `library` as federation/1 carries it: its top-level
    /// items, a show's seasons and episodes, an artist's albums and their
    /// tracks.
    async fn walk(&self, library: &LibraryId) -> Result<Vec<RemoteItem>, CatalogError> {
        // The external ids kept, read once for the walk; those read anew
        // are kept at its end, together.
        let mut ids = Ids {
            library: library.to_string(),
            kept: self
                .store()
                .external_ids_of(&library.to_string())
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "cannot read the kept external ids");
                    HashMap::new()
                }),
            read: Vec::new(),
        };
        let walked = self.walk_with(library, &mut ids).await;
        if let Err(e) = self.store().keep_all_external_ids(&ids.read).await {
            tracing::warn!(error = %e, "cannot keep the external ids read");
        }
        walked
    }

    async fn walk_with(
        &self,
        library: &LibraryId,
        ids: &mut Ids,
    ) -> Result<Vec<RemoteItem>, CatalogError> {
        let mut top = Vec::new();
        loop {
            let page = self
                .catalog
                .items(
                    library,
                    Page {
                        offset: u32::try_from(top.len()).unwrap_or(u32::MAX),
                        limit: WALK_PAGE,
                    },
                )
                .await?;
            let got = page.items.len();
            top.extend(page.items);
            if got == 0 || top.len() >= page.total as usize {
                break;
            }
        }
        let mut walked = Vec::new();
        for item in top {
            match item {
                Item::Show(_) => {
                    let show = self.catalog.item(item.id()).await?;
                    walked.push(remote(&show.item, show.external_ids));
                    for season in show.children {
                        walked.push(remote(&season, Vec::new()));
                    }
                    for episode in self.catalog.episodes(item.id()).await? {
                        walked.push(self.leaf(&episode, ids).await?);
                    }
                }
                Item::Artist(_) => {
                    let artist = self.catalog.item(item.id()).await?;
                    walked.push(remote(&artist.item, artist.external_ids));
                    for album in artist.children {
                        let album = self.catalog.item(album.id()).await?;
                        walked.push(remote(&album.item, album.external_ids));
                        for track in album.children {
                            walked.push(self.leaf(&track, ids).await?);
                        }
                    }
                }
                _ => walked.push(self.leaf(&item, ids).await?),
            }
        }
        Ok(walked)
    }

    /// A film, episode or track as sent, with its external ids: the ones
    /// kept while the rest of it is unchanged, else the catalog's now. A
    /// listing has none, and asking for each at every sync would cost a
    /// request per item.
    async fn leaf(&self, item: &Item, ids: &mut Ids) -> Result<RemoteItem, CatalogError> {
        let id = item.id().to_string();
        let shallow = remote(item, Vec::new()).hash;
        let external_ids = match ids.kept.get(&id).filter(|(was, _)| *was == shallow) {
            Some((_, kept)) => kept.clone(),
            None => {
                let read = self.catalog.external_ids(item.id()).await?;
                ids.read.push(KeptIds {
                    item: id,
                    library: ids.library.clone(),
                    shallow,
                    external_ids: read.clone(),
                });
                read
            }
        };
        Ok(remote(item, external_ids))
    }

    /// Answer pair `peer`'s `sync` of `library`, walked as `walked`: what
    /// changed since its cursor, or the whole library for a cursor not
    /// kept, and keep what is sent.
    async fn diff(
        &self,
        peer: &PeerRecord,
        sync: &SyncLibrary,
        walked: &[RemoteItem],
    ) -> Result<SyncAnswer, StoreError> {
        let store = self.store();
        let base = match &sync.since {
            Some(since) => store.sync_state(&peer.id, &sync.library, since).await?,
            None => None,
        };
        let since = base.as_ref().and(sync.since.as_deref());
        let sent: Vec<SentItem> = walked
            .iter()
            .map(|item| SentItem {
                item: item.id.clone(),
                hash: item.hash.clone(),
                artwork: item.artwork.iter().map(|a| a.id.clone()).collect(),
            })
            .collect();
        let Some(base) = base else {
            let cursor = Uuid::now_v7().simple().to_string();
            store
                .keep_sync_state(&peer.id, &sync.library, None, &cursor, &sent)
                .await?;
            return Ok(SyncAnswer {
                mode: SyncMode::Snapshot,
                cursor,
                items: walked.to_vec(),
                removed: Vec::new(),
            });
        };
        let found: HashSet<&str> = walked.iter().map(|i| i.id.as_str()).collect();
        let removed: Vec<String> = base
            .keys()
            .filter(|id| !found.contains(id.as_str()))
            .cloned()
            .collect();
        let changed: Vec<RemoteItem> = walked
            .iter()
            .filter(|i| base.get(&i.id) != Some(&i.hash))
            .cloned()
            .collect();
        // Nothing changed: the pair's cursor stands, and nothing is written
        // but the forgetting of a state it never took up.
        let cursor = if changed.is_empty() && removed.is_empty() {
            since.expect("a base is a kept cursor's").to_owned()
        } else {
            Uuid::now_v7().simple().to_string()
        };
        store
            .keep_sync_state(&peer.id, &sync.library, since, &cursor, &sent)
            .await?;
        Ok(SyncAnswer {
            mode: SyncMode::Changes,
            cursor,
            items: changed,
            removed,
        })
    }
}

#[async_trait::async_trait]
impl Shares for Federation {
    async fn libraries(&self, server_id: &ServerId) -> Result<Vec<SharedLibrary>, Unshared> {
        let peer = self.pair(server_id).await?;
        let granted = self
            .grants(&peer.id)
            .await
            .map_err(|e| unavailable("libraries", e))?;
        let served: HashMap<String, Library> = self
            .shareable()
            .await
            .map_err(|e| unavailable("libraries", e))?
            .into_iter()
            .map(|l| (l.id.to_string(), l))
            .collect();
        // A library shared but no longer served is not listed: the pair
        // drops it as it would a revoked one.
        Ok(granted
            .iter()
            .filter_map(|g| served.get(&g.library))
            .map(|l| SharedLibrary {
                id: l.id.to_string(),
                title: l.title.clone(),
                kind: library_kind(l.kind),
            })
            .collect())
    }

    async fn sync(&self, server_id: &ServerId, sync: &SyncLibrary) -> Result<SyncAnswer, Unshared> {
        let peer = self.pair(server_id).await?;
        let library = self.shared_library(&peer, &sync.library).await?;
        let _syncing = self
            .answering
            .syncing
            .lock(&format!("{}\n{}", peer.id, sync.library))
            .await;
        let walked = self
            .walked(&library.id)
            .await
            .map_err(|e| unavailable("sync", e))?;
        let answer = self
            .diff(&peer, sync, &walked)
            .await
            .map_err(|e| unavailable("sync", e))?;
        // Revoked while it was walked: none of it is sent.
        self.shared_library(&peer, &sync.library).await?;
        Ok(answer)
    }

    async fn artwork(&self, server_id: &ServerId, id: &str) -> Result<(String, Vec<u8>), Unshared> {
        let peer = self.pair(server_id).await?;
        if !self.answering.allowed(server_id.as_str(), Ask::Artwork) {
            tracing::debug!(server_id = %server_id, "a paired server asks for images too fast");
            return Err(Unshared::Unavailable);
        }
        let libraries = self
            .store()
            .sent_artwork_libraries(&peer.id, id)
            .await
            .map_err(|e| unavailable("artwork", e))?;
        // Only an image of an item this server sent the pair, from a
        // library still shared with it.
        let mut shared = false;
        for library in &libraries {
            if self
                .store()
                .is_granted(&peer.id, library)
                .await
                .map_err(|e| unavailable("artwork", e))?
            {
                shared = true;
                break;
            }
        }
        if !shared {
            return Err(Unshared::NotShared);
        }
        let artwork: ArtworkId = id.parse().map_err(|_| Unshared::NotShared)?;
        match self.catalog.artwork(&artwork).await {
            Ok(image) => Ok((image.content_type, image.bytes)),
            Err(CatalogError::NotFound(_) | CatalogError::NotServed(_)) => Err(Unshared::NotFound),
            Err(e) => Err(unavailable("artwork", e)),
        }
    }

    async fn assert_user(
        &self,
        server_id: &ServerId,
        assertion: &AssertUser,
    ) -> Result<TokenGranted, AssertionRefused> {
        if !self.answering.allowed(server_id.as_str(), Ask::Assertion) {
            tracing::info!(server_id = %server_id, "a paired server vouches too fast");
            return Err(AssertionRefused::Unavailable);
        }
        self.mint(server_id, assertion).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pair's images come in a burst of the bucket's size, then at its
    /// pace; each pair has its own, and its assertions are paced apart.
    #[test]
    fn each_pair_s_images_are_paced_by_its_own_bucket() {
        let answering = Answering::default();
        for _ in 0..ARTWORK_ASKS.capacity {
            assert!(answering.allowed("sb", Ask::Artwork));
        }
        assert!(!answering.allowed("sb", Ask::Artwork));
        assert!(answering.allowed("sc", Ask::Artwork));
        assert!(answering.allowed("sb", Ask::Assertion));
    }

    /// A walk past its reuse goes at the tick, though no pair asks for
    /// its library again; one within it stays.
    #[test]
    fn walks_past_their_reuse_go_at_the_tick() {
        let answering = Answering::default();
        let old = Instant::now()
            .checked_sub(WALK_REUSE + Duration::from_secs(1))
            .expect("the clock is past it");
        answering
            .walks
            .lock()
            .insert("plex:1".into(), (old, Walk::default()));
        answering
            .walks
            .lock()
            .insert("plex:2".into(), (Instant::now(), Walk::default()));
        answering.prune_walks();
        let left: Vec<String> = answering.walks.lock().keys().cloned().collect();
        assert_eq!(left, ["plex:2"]);
    }
}
