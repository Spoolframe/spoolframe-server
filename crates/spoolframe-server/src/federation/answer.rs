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

use spoolframe_catalog::{CatalogError, Facts, Page};
use spoolframe_federation::{Shares, SyncAnswer, Unshared};
use spoolframe_protocol::ServerId;
use spoolframe_protocol::federation::{
    AssertUser, AssertionRefused, RemoteItem, RemoteLibraryKind, SharedLibrary,
    SubtitleDownloadAsk, SubtitleRemovalAsk, SubtitleSearchAsk, SyncLibrary, SyncMode,
    TokenGranted,
};
use spoolframe_types::{
    ArtworkId, Item, Library, LibraryId, LibraryKind, SubtitleDownloaded, SubtitleSearch,
};
use uuid::Uuid;

use super::items::remote;
use super::{Federation, Limits};
use crate::auth::{Bucket, TokenBucket};
use crate::keyed_locks::KeyedLocks;
use crate::store::StoreError;
use crate::store::federation::PeerRecord;
use crate::store::federation_sync::{Kept, KeptIds, SentItem};

/// How many top-level items a walk asks the catalog for at once.
const WALK_PAGE: u32 = 200;

/// Each pair's images: a first sync fetches every poster, so a burst of
/// [`Limits::artwork_burst`], then a hundred a second.
const ARTWORK_REFILL: Duration = Duration::from_millis(10);

/// Each pair's assertions: one per play its users start, a burst of
/// [`Limits::assertion_burst`], then one a second.
const ASSERTION_REFILL: Duration = Duration::from_secs(1);

/// Each pair's subtitle searches and downloads (P8-07): each asks a
/// subtitle site with this server's key, so a burst of
/// [`Limits::subtitle_burst`], then one every ten seconds.
const SUBTITLE_REFILL: Duration = Duration::from_secs(10);

/// Each pair's dear answers (M8-02, Z8-02): a sync naming no cursor this
/// server keeps is answered with the whole library, and one naming a
/// cursor kept but older than the last it was given with everything
/// since, its state written whole again; a pair could ask for either
/// again and again. A pair's first syncs, a cache it lost, or a reply it
/// lost, take a burst of sixteen; then one every ten minutes. A sync
/// naming the cursor it was given last is not held to it.
const SNAPSHOTS: Bucket = Bucket {
    capacity: 16,
    refill_every: Duration::from_secs(600),
};

/// The external ids a walk knows: those kept, by item, with the hash of
/// the rest of it then; and those it read anew.
struct Ids {
    /// The library walked.
    library: String,
    kept: HashMap<String, Kept>,
    read: Vec<KeptIds>,
}

/// What a film's own record says beyond its listing, kept with its
/// external ids: every genre (a listing has the first few) and its
/// dynamic range (a listing has no streams).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Details {
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default)]
    dynamic_range: Option<spoolframe_types::DynamicRange>,
}

/// A library's items, walked: shared by the pairs it answers.
type Walk = Arc<Vec<RemoteItem>>;

/// What paces the pairs' asks (H2): one walk of a library at a time, kept
/// for [`Limits::walk_reuse`]; one sync per pair and library at a time;
/// and each pair's images and assertions, by token bucket. The limits are
/// [`Federation::limits`]'s, read as each ask comes.
#[derive(Debug, Default)]
pub(crate) struct Answering {
    /// Held while a library is walked, by library.
    walking: KeyedLocks,
    /// Each library's last walk, and when it was made.
    walks: parking_lot::Mutex<HashMap<String, (Instant, Walk)>>,
    /// Held while a pair's sync of a library is answered, by pair and
    /// library: its kept state is read and written by one at a time.
    syncing: KeyedLocks,
    /// Each pair's buckets, by its server id: images, then assertions,
    /// with the sizes they were made at.
    asks: parking_lot::Mutex<HashMap<String, Arc<PairBuckets>>>,
}

/// A pair's buckets, and what they were made as: a change of a burst
/// makes them again, full.
#[derive(Debug)]
struct PairBuckets {
    made: (Bucket, Bucket, Bucket),
    artwork: TokenBucket,
    assertions: TokenBucket,
    subtitles: TokenBucket,
    snapshots: TokenBucket,
}

/// The buckets `limits` make a pair's: images, assertions, then subtitle
/// asks.
fn buckets(limits: &Limits) -> (Bucket, Bucket, Bucket) {
    (
        Bucket {
            capacity: limits.artwork_burst,
            refill_every: ARTWORK_REFILL,
        },
        Bucket {
            capacity: limits.assertion_burst,
            refill_every: ASSERTION_REFILL,
        },
        Bucket {
            capacity: limits.subtitle_burst,
            refill_every: SUBTITLE_REFILL,
        },
    )
}

/// A pair's buckets as `made`, full.
fn fresh(made: (Bucket, Bucket, Bucket)) -> Arc<PairBuckets> {
    Arc::new(PairBuckets {
        made,
        artwork: TokenBucket::new(made.0),
        assertions: TokenBucket::new(made.1),
        subtitles: TokenBucket::new(made.2),
        snapshots: TokenBucket::new(SNAPSHOTS),
    })
}

/// What a pair asks that is paced by bucket.
#[derive(Debug, Clone, Copy)]
pub(super) enum Ask {
    Artwork,
    Assertion,
    /// A subtitle search or download (P8-07).
    Subtitles,
    /// A sync answered with the whole library (M8-02).
    Snapshot,
}

impl Answering {
    /// Whether pair `peer` may ask `ask` now, under `limits`. Its buckets
    /// are made again, full, when a burst changed.
    pub(super) fn allowed(&self, peer: &str, ask: Ask, limits: &Limits) -> bool {
        let made = buckets(limits);
        // Taken under the map's lock: the tick's pruning never lets a
        // bucket go between its lookup and the take, which would give the
        // pair a full one again (the fourth Codex pass on G8-05).
        let mut asks = self.asks.lock();
        let buckets = asks.entry(peer.to_owned()).or_insert_with(|| fresh(made));
        if buckets.made != made {
            *buckets = fresh(made);
        }
        Self::bucket(buckets, ask).take(Instant::now()).is_ok()
    }

    /// Give pair `peer` back the token its `ask` took in [`allowed`], for
    /// an ask that was then not answered. A pair whose buckets are gone
    /// (full again, pruned) has nothing to get back.
    ///
    /// [`allowed`]: Answering::allowed
    fn give_back(&self, peer: &str, ask: Ask) {
        if let Some(buckets) = self.asks.lock().get(peer) {
            Self::bucket(buckets, ask).give_back();
        }
    }

    fn bucket(buckets: &PairBuckets, ask: Ask) -> &TokenBucket {
        match ask {
            Ask::Artwork => &buckets.artwork,
            Ask::Assertion => &buckets.assertions,
            Ask::Subtitles => &buckets.subtitles,
            Ask::Snapshot => &buckets.snapshots,
        }
    }

    /// The walk of `library` made within `reuse`, if there is one.
    fn reused(&self, library: &str, reuse: Duration) -> Option<Walk> {
        self.walks
            .lock()
            .get(library)
            .filter(|(at, _)| at.elapsed() < reuse)
            .map(|(_, walk)| walk.clone())
    }

    /// Let the walks past `reuse` go: at each replica tick, so one of a
    /// library no pair asks for again is not held.
    pub(super) fn prune_walks(&self, reuse: Duration) {
        self.walks.lock().retain(|_, (at, _)| at.elapsed() < reuse);
    }

    /// Let the pairs' buckets that are full again go, at the same tick: a
    /// full one is what a pair's next ask would make anew, so nothing is
    /// lost, and a pair unpaired leaves none behind (G8-05).
    pub(super) fn prune_asks(&self, now: Instant) {
        self.asks.lock().retain(|_, b| {
            ![&b.artwork, &b.assertions, &b.subtitles, &b.snapshots]
                .iter()
                .all(|bucket| bucket.full(now))
        });
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
    /// The live pair pinned under `server_id`, at `endpoint`: a
    /// connection's standing said so when it said hello, and it may have
    /// been unpaired, or moved to another key, since (X8-01). A request
    /// from a key that is no longer the pair's is refused as not shared,
    /// as one from no pair is: the pin is the key's proof, read for each
    /// request.
    async fn pair(
        &self,
        server_id: &ServerId,
        endpoint: iroh::EndpointId,
    ) -> Result<PeerRecord, Unshared> {
        self.live_pair_at(server_id, endpoint)
            .await
            .map_err(|e| unavailable("request", e))?
            .ok_or(Unshared::NotShared)
    }

    /// The live pair pinned under `server_id`, if it is pinned to
    /// `endpoint` now: what a request from that key is answered as.
    pub(super) async fn live_pair_at(
        &self,
        server_id: &ServerId,
        endpoint: iroh::EndpointId,
    ) -> Result<Option<PeerRecord>, StoreError> {
        self.pair_at(server_id.as_str(), &endpoint.to_string())
            .await
    }

    /// Library `id`, if it is shared with `peer` and served now.
    async fn shared_library(&self, peer: &PeerRecord, id: &str) -> Result<Library, Unshared> {
        self.granted(&peer.id, id)
            .await
            .map_err(|e| unavailable("sync", e))?
            .ok_or(Unshared::NotShared)
    }

    /// `library` walked, or its walk made within the reuse: one walk at a
    /// time per library, for every pair.
    async fn walked(&self, library: &LibraryId) -> Result<Walk, CatalogError> {
        let key = library.to_string();
        let _walking = self.answering.walking.lock(&key).await;
        if let Some(walk) = self.answering.reused(&key, self.limits().walk_reuse) {
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
        // The top of the library, with what its listing says of each to
        // sort and filter by.
        let mut top = Vec::new();
        loop {
            let (page, facts) = self
                .catalog
                .items_with_facts(
                    library,
                    Page {
                        offset: u32::try_from(top.len()).unwrap_or(u32::MAX),
                        limit: WALK_PAGE,
                    },
                )
                .await?;
            let got = page.items.len();
            let mut facts = facts.into_iter();
            top.extend(
                page.items
                    .into_iter()
                    .map(|item| (item, facts.next().unwrap_or_default())),
            );
            if got == 0 || top.len() >= page.total as usize {
                break;
            }
        }
        let none = Facts::default();
        let mut walked = Vec::new();
        for (item, facts) in top {
            match item {
                Item::Show(_) => {
                    let show = self.catalog.item(item.id()).await?;
                    walked.push(remote(&show.item, show.external_ids, &facts));
                    for season in show.children {
                        walked.push(remote(&season, Vec::new(), &none));
                    }
                    for episode in self.catalog.episodes(item.id()).await? {
                        walked.push(self.leaf(&episode, &none, ids).await?);
                    }
                }
                Item::Artist(_) => {
                    let artist = self.catalog.item(item.id()).await?;
                    walked.push(remote(&artist.item, artist.external_ids, &facts));
                    for album in artist.children {
                        let album = self.catalog.item(album.id()).await?;
                        walked.push(remote(&album.item, album.external_ids, &none));
                        for track in album.children {
                            walked.push(self.leaf(&track, &none, ids).await?);
                        }
                    }
                }
                _ => walked.push(self.leaf(&item, &facts, ids).await?),
            }
        }
        Ok(walked)
    }

    /// A film, episode or track as sent, with its external ids: the ones
    /// kept while the rest of it is unchanged, else the catalog's now. A
    /// listing has none, and asking for each at every sync would cost a
    /// request per item. A film's own record also says what its listing
    /// does not (every genre, its dynamic range), read and kept with them;
    /// one kept from before that is read once more.
    async fn leaf(
        &self,
        item: &Item,
        listed: &Facts,
        ids: &mut Ids,
    ) -> Result<RemoteItem, CatalogError> {
        let film = matches!(item, Item::Movie(_));
        let id = item.id().to_string();
        let shallow = remote(item, Vec::new(), listed).hash;
        let kept = ids
            .kept
            .get(&id)
            .filter(|(was, _, details)| *was == shallow && (!film || details.is_some()));
        let (external_ids, details) = match kept {
            Some((_, kept, details)) => (
                kept.clone(),
                details
                    .as_deref()
                    .and_then(|d| serde_json::from_str::<Details>(d).ok())
                    .unwrap_or_default(),
            ),
            None if film => {
                let (read, record) = self.catalog.details(item.id()).await?;
                let details = Details {
                    genres: record.genres,
                    dynamic_range: record.dynamic_range,
                };
                ids.read.push(KeptIds {
                    item: id,
                    library: ids.library.clone(),
                    shallow,
                    external_ids: read.clone(),
                    details: Some(serde_json::to_string(&details).expect("details serialize")),
                });
                (read, details)
            }
            None => {
                let read = self.catalog.external_ids(item.id()).await?;
                ids.read.push(KeptIds {
                    item: id,
                    library: ids.library.clone(),
                    shallow,
                    external_ids: read.clone(),
                    details: None,
                });
                (read, Details::default())
            }
        };
        let facts = Facts {
            genres: if details.genres.is_empty() {
                listed.genres.clone()
            } else {
                details.genres
            },
            dynamic_range: details.dynamic_range.or(listed.dynamic_range),
            ..listed.clone()
        };
        Ok(remote(item, external_ids, &facts))
    }

    /// Answer pair `peer`'s `sync` of `library`, walked as `walked`: what
    /// changed since its cursor's state `base`, or the whole library for
    /// a cursor not kept, and keep what is sent.
    async fn diff(
        &self,
        peer: &PeerRecord,
        sync: &SyncLibrary,
        base: Option<HashMap<String, String>>,
        walked: &Walk,
    ) -> Result<SyncAnswer, StoreError> {
        let store = self.store();
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
            // The walk itself, not a copy of it (Y8-04): every snapshot
            // in hand of this walk is the one vector.
            return Ok(SyncAnswer {
                mode: SyncMode::Snapshot,
                cursor,
                items: walked.clone(),
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
            items: Arc::new(changed),
            removed,
        })
    }
}

#[async_trait::async_trait]
impl Shares for Federation {
    async fn libraries(
        &self,
        server_id: &ServerId,
        endpoint: iroh::EndpointId,
    ) -> Result<Vec<SharedLibrary>, Unshared> {
        let peer = self.pair(server_id, endpoint).await?;
        // The store and the catalog at once: neither waits on the other.
        let (granted, served) = tokio::join!(self.grants(&peer.id), self.shareable());
        let granted = granted.map_err(|e| unavailable("libraries", e))?;
        let served: HashMap<String, Library> = served
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

    async fn sync(
        &self,
        server_id: &ServerId,
        endpoint: iroh::EndpointId,
        sync: &SyncLibrary,
    ) -> Result<SyncAnswer, Unshared> {
        let peer = self.pair(server_id, endpoint).await?;
        let library = self.shared_library(&peer, &sync.library).await?;
        #[cfg(test)]
        self.holds.answering.here().await;
        let _syncing = self
            .answering
            .syncing
            .lock(&format!("{}\n{}", peer.id, sync.library))
            .await;
        // Read under the lock, before the walk: a whole library past the
        // pair's pace is refused before anything is walked (M8-02).
        let base = match &sync.since {
            Some(since) => self
                .store()
                .sync_state(&peer.id, &sync.library, since)
                .await
                .map_err(|e| unavailable("sync", e))?,
            None => None,
        };
        // A kept cursor older than the last one given: a reply lost, or
        // a pair asking for the same changes again (Z8-02).
        let replay = match (&base, &sync.since) {
            (Some(_), Some(since)) => {
                self.store()
                    .newest_sync_cursor(&peer.id, &sync.library)
                    .await
                    .map_err(|e| unavailable("sync", e))?
                    .as_deref()
                    != Some(since.as_str())
            }
            _ => false,
        };
        let whole = base.is_none() || replay;
        if whole
            && !self
                .answering
                .allowed(server_id.as_str(), Ask::Snapshot, &self.limits())
        {
            tracing::info!(
                server_id = %server_id,
                replay,
                "a paired server asks for whole libraries, or old cursors, too often"
            );
            return Err(Unshared::Unavailable);
        }
        let answered = async {
            let walked = self
                .walked(&library.id)
                .await
                .map_err(|e| unavailable("sync", e))?;
            // The pair and the grant are read again after the walk, which
            // can take minutes: revoked, or the pair moved to another key,
            // meanwhile, none of it is sent, nor kept as sent to the pair
            // (X8-01). A change between this read and `diff`'s write is
            // not caught here, and heals itself: what `diff` keeps is
            // which items were sent, and a revoked pair's state goes with
            // the grant, a moved key's with the next sync the pair's new
            // key makes, whose cursor it never saw.
            let peer = self.pair(server_id, endpoint).await?;
            self.shared_library(&peer, &sync.library).await?;
            self.diff(&peer, sync, base, &walked)
                .await
                .map_err(|e| unavailable("sync", e))
        }
        .await;
        // The pace is for answers: a walk that failed, or a grant revoked
        // while it ran, sent the pair nothing, and costs it no token.
        if whole && answered.is_err() {
            self.answering.give_back(server_id.as_str(), Ask::Snapshot);
        }
        answered
    }

    async fn artwork(
        &self,
        server_id: &ServerId,
        endpoint: iroh::EndpointId,
        id: &str,
    ) -> Result<(String, Vec<u8>), Unshared> {
        let peer = self.pair(server_id, endpoint).await?;
        // Before the shared check, on purpose: asking for images that are
        // not shared is a probe, and is paced as any ask is.
        if !self
            .answering
            .allowed(server_id.as_str(), Ask::Artwork, &self.limits())
        {
            tracing::debug!(server_id = %server_id, "a paired server asks for images too fast");
            return Err(Unshared::Unavailable);
        }
        // Only an image of an item this server sent the pair, from a
        // library still shared with it.
        if !self
            .store()
            .artwork_shared(&peer.id, id)
            .await
            .map_err(|e| unavailable("artwork", e))?
        {
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
        endpoint: iroh::EndpointId,
        assertion: &AssertUser,
    ) -> Result<TokenGranted, AssertionRefused> {
        if !self
            .answering
            .allowed(server_id.as_str(), Ask::Assertion, &self.limits())
        {
            tracing::info!(server_id = %server_id, "a paired server vouches too fast");
            return Err(AssertionRefused::Unavailable);
        }
        self.mint(server_id, endpoint, assertion).await
    }

    async fn search_subtitles(
        &self,
        server_id: &ServerId,
        endpoint: iroh::EndpointId,
        ask: &SubtitleSearchAsk,
    ) -> Result<SubtitleSearch, spoolframe_protocol::Error> {
        self.answer_search(server_id, endpoint, ask).await
    }

    async fn download_subtitle(
        &self,
        server_id: &ServerId,
        endpoint: iroh::EndpointId,
        ask: &SubtitleDownloadAsk,
    ) -> Result<SubtitleDownloaded, spoolframe_protocol::Error> {
        self.answer_download(server_id, endpoint, ask).await
    }

    async fn remove_subtitle_download(
        &self,
        server_id: &ServerId,
        endpoint: iroh::EndpointId,
        ask: &SubtitleRemovalAsk,
    ) -> Result<(), spoolframe_protocol::Error> {
        self.answer_removal(server_id, endpoint, ask).await
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
        let limits = Limits::default();
        for _ in 0..limits.artwork_burst {
            assert!(answering.allowed("sb", Ask::Artwork, &limits));
        }
        assert!(!answering.allowed("sb", Ask::Artwork, &limits));
        assert!(answering.allowed("sc", Ask::Artwork, &limits));
        assert!(answering.allowed("sb", Ask::Assertion, &limits));
    }

    /// A pair's subtitle asks (P8-07) come in a burst of their own, apart
    /// from its images and its assertions.
    #[test]
    fn a_pair_s_subtitle_asks_are_paced_apart() {
        let answering = Answering::default();
        let limits = Limits {
            subtitle_burst: 2,
            ..Limits::default()
        };
        assert!(answering.allowed("sb", Ask::Subtitles, &limits));
        assert!(answering.allowed("sb", Ask::Subtitles, &limits));
        assert!(!answering.allowed("sb", Ask::Subtitles, &limits));
        assert!(answering.allowed("sb", Ask::Artwork, &limits));
        assert!(answering.allowed("sb", Ask::Assertion, &limits));
        assert!(answering.allowed("sc", Ask::Subtitles, &limits));
    }

    /// A pair's whole-library answers (M8-02) come in a burst of their
    /// own, apart from its other asks and from another pair's.
    #[test]
    fn a_pair_s_whole_libraries_are_paced_apart() {
        let answering = Answering::default();
        let limits = Limits::default();
        for _ in 0..SNAPSHOTS.capacity {
            assert!(answering.allowed("sb", Ask::Snapshot, &limits));
        }
        assert!(!answering.allowed("sb", Ask::Snapshot, &limits));
        assert!(answering.allowed("sb", Ask::Artwork, &limits));
        assert!(answering.allowed("sc", Ask::Snapshot, &limits));
    }

    /// A film's own record (every genre, its range) is read once, kept
    /// with its external ids and sent from there at the next walk; one
    /// kept from before it had them is read once more.
    #[tokio::test]
    async fn a_film_s_details_are_read_once_and_kept() {
        use super::super::tokens::tests::sharing_from;
        use crate::test_support::FakeCatalog;
        let catalog = Arc::new(FakeCatalog::new("/nowhere/movie.mkv".into()));
        let (store, federation, _) = sharing_from("fed-walk-details", catalog.clone()).await;
        let library: LibraryId = "plex:1".parse().unwrap();
        let film: spoolframe_types::MediaId = "plex:movie:1".parse().unwrap();
        let sent = |walked: &[RemoteItem]| {
            let item = walked
                .iter()
                .find(|i| i.id == film.to_string())
                .expect("the film is walked")
                .clone();
            (item.genres, item.dynamic_range)
        };
        let expected = (
            vec!["Horror".to_owned(), "Science Fiction".to_owned()],
            Some(spoolframe_types::DynamicRange::Hdr),
        );
        catalog.take_calls(&film);
        let first = federation.walk(&library).await.unwrap();
        assert_eq!(sent(&first), expected);
        assert_eq!(catalog.take_calls(&film), ["details"]);
        let again = federation.walk(&library).await.unwrap();
        assert_eq!(sent(&again), expected);
        assert_eq!(catalog.take_calls(&film), Vec::<&str>::new());
        // Kept before details were: read again, once.
        let kept = store.external_ids_of("plex:1").await.unwrap();
        let (shallow, ids, _) = kept[&film.to_string()].clone();
        store
            .keep_all_external_ids(&[KeptIds {
                item: film.to_string(),
                library: "plex:1".into(),
                shallow,
                external_ids: ids,
                details: None,
            }])
            .await
            .unwrap();
        assert_eq!(sent(&federation.walk(&library).await.unwrap()), expected);
        assert_eq!(catalog.take_calls(&film), ["details"]);
        federation.walk(&library).await.unwrap();
        assert_eq!(catalog.take_calls(&film), Vec::<&str>::new());
    }

    /// M8-02: a pair that names no cursor kept, again and again, is
    /// refused the whole library past its burst, before any walk; a sync
    /// naming the cursor it was given is still answered with the changes.
    #[tokio::test]
    async fn a_pair_s_whole_library_syncs_past_the_burst_are_refused() {
        use super::super::tokens::tests::{bob_s_key, sharing};
        let (_store, federation, bob_s) = sharing("fed-sync-snapshots").await;
        let unknown = SyncLibrary {
            library: "plex:1".into(),
            since: Some("not-a-kept-cursor".into()),
        };
        let mut cursor = String::new();
        for _ in 0..SNAPSHOTS.capacity {
            let answer = Shares::sync(&federation, &bob_s, bob_s_key(), &unknown)
                .await
                .unwrap();
            assert_eq!(answer.mode, SyncMode::Snapshot);
            cursor = answer.cursor;
        }
        assert_eq!(
            Shares::sync(&federation, &bob_s, bob_s_key(), &unknown)
                .await
                .map(|a| a.mode),
            Err(Unshared::Unavailable)
        );
        let kept = SyncLibrary {
            library: "plex:1".into(),
            since: Some(cursor),
        };
        assert_eq!(
            Shares::sync(&federation, &bob_s, bob_s_key(), &kept)
                .await
                .map(|a| a.mode),
            Ok(SyncMode::Changes)
        );
    }

    /// A whole-library sync that the pair's key moved under, while it
    /// ran, sent nothing and keeps its token; one that is answered does
    /// not.
    #[tokio::test]
    async fn a_whole_library_sync_that_fails_gives_its_token_back() {
        use super::super::tokens::tests::{bob_s_key, sharing};
        let (store, federation, bob_s) = sharing("fed-sync-refund").await;
        let limits = federation.limits();
        // Fifteen of sixteen used up; one is left.
        for _ in 1..SNAPSHOTS.capacity {
            assert!(
                federation
                    .answering
                    .allowed(bob_s.as_str(), Ask::Snapshot, &limits)
            );
        }
        let peer = store
            .live_peer_by_server_id(bob_s.as_str())
            .await
            .unwrap()
            .unwrap();
        let new = iroh::SecretKey::from_bytes(&[12; 32]).public();
        store
            .hold_peer_key(&peer.id, &new.to_string())
            .await
            .unwrap();
        let whole = SyncLibrary {
            library: "plex:1".into(),
            since: None,
        };
        let (held, go) = federation.holds.answering.next();
        let confirm = async {
            held.await.unwrap();
            federation
                .confirm_key(&peer.id, &new.to_string(), "alice")
                .await
                .unwrap();
            go.send(()).unwrap();
        };
        let (failed, ()) = tokio::join!(
            Shares::sync(&federation, &bob_s, bob_s_key(), &whole),
            confirm
        );
        assert_eq!(failed.map(|a| a.mode), Err(Unshared::NotShared));
        // The token is back: the new key's whole-library sync is answered
        // with it, and takes the last.
        assert_eq!(
            Shares::sync(&federation, &bob_s, new, &whole)
                .await
                .map(|a| a.mode),
            Ok(SyncMode::Snapshot)
        );
        assert!(
            !federation
                .answering
                .allowed(bob_s.as_str(), Ask::Snapshot, &limits)
        );
    }

    /// Z8-02: a pair that keeps naming a cursor older than the last it was
    /// given is answered the changes since it, each time a new state,
    /// paced as whole libraries are; the cursor it was given last is
    /// answered as ever.
    #[tokio::test]
    async fn a_pair_replaying_an_old_cursor_is_paced() {
        use super::super::tokens::tests::{bob_s_key, sharing_from};
        use crate::test_support::{FakeCatalog, movie};
        let catalog = Arc::new(FakeCatalog::new("/nowhere/movie.mkv".into()));
        let (store, federation, bob_s) = sharing_from("fed-sync-replay", catalog.clone()).await;
        federation.set_limits(Limits {
            walk_reuse: Duration::ZERO,
            ..Limits::default()
        });
        let sync = |since: Option<String>| SyncLibrary {
            library: "plex:1".into(),
            since,
        };
        let asked = |s: SyncLibrary| {
            let federation = &federation;
            let bob_s = &bob_s;
            async move { Shares::sync(federation, bob_s, bob_s_key(), &s).await }
        };
        let old = asked(sync(None)).await.unwrap().cursor;
        catalog.remove(movie().id());
        let newest = asked(sync(Some(old.clone()))).await.unwrap();
        assert_eq!(newest.mode, SyncMode::Changes);
        assert_ne!(newest.cursor, old);
        // The snapshot took one of sixteen; fifteen replays take the rest.
        for _ in 1..SNAPSHOTS.capacity {
            assert_eq!(
                asked(sync(Some(old.clone()))).await.map(|a| a.mode),
                Ok(SyncMode::Changes)
            );
        }
        assert_eq!(
            asked(sync(Some(old.clone()))).await.map(|a| a.mode),
            Err(Unshared::Unavailable)
        );
        let peer = store
            .live_peer_by_server_id(bob_s.as_str())
            .await
            .unwrap()
            .unwrap();
        let last = store
            .newest_sync_cursor(&peer.id, "plex:1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            asked(sync(Some(last))).await.map(|a| a.mode),
            Ok(SyncMode::Changes)
        );
    }

    /// A walk past its reuse goes at the tick, though no pair asks for
    /// its library again; one within it stays.
    #[test]
    fn walks_past_their_reuse_go_at_the_tick() {
        let answering = Answering::default();
        let old = Instant::now()
            .checked_sub(Limits::default().walk_reuse + Duration::from_secs(1))
            .expect("the clock is past it");
        answering
            .walks
            .lock()
            .insert("plex:1".into(), (old, Walk::default()));
        answering
            .walks
            .lock()
            .insert("plex:2".into(), (Instant::now(), Walk::default()));
        answering.prune_walks(Limits::default().walk_reuse);
        let left: Vec<String> = answering.walks.lock().keys().cloned().collect();
        assert_eq!(left, ["plex:2"]);
    }

    /// G8-05: a pair's buckets go at the tick once they are full again,
    /// and stay while any is not.
    #[test]
    fn a_pair_s_buckets_go_once_full_again() {
        let answering = Answering::default();
        let limits = Limits::default();
        assert!(answering.allowed("sb", Ask::Snapshot, &limits));
        assert!(answering.allowed("sc", Ask::Subtitles, &limits));
        answering.prune_asks(Instant::now());
        assert_eq!(answering.asks.lock().len(), 2);
        answering.prune_asks(Instant::now() + SNAPSHOTS.refill_every);
        let left: Vec<String> = answering.asks.lock().keys().cloned().collect();
        assert_eq!(left, Vec::<String>::new());
        assert!(answering.allowed("sb", Ask::Snapshot, &limits));
    }

    /// A burst raised by the settings (P8-04) holds at once, for a pair
    /// that had used its old one up.
    #[test]
    fn a_raised_burst_holds_at_once() {
        let answering = Answering::default();
        let limits = Limits::default();
        let first = limits.assertion_burst;
        for _ in 0..first {
            assert!(answering.allowed("sb", Ask::Assertion, &limits));
        }
        assert!(!answering.allowed("sb", Ask::Assertion, &limits));
        let limits = Limits {
            assertion_burst: first * 2,
            ..limits
        };
        for _ in 0..first * 2 {
            assert!(answering.allowed("sb", Ask::Assertion, &limits));
        }
        assert!(!answering.allowed("sb", Ask::Assertion, &limits));
    }
}
