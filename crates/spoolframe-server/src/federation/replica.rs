//! The side a library is shared with (P8-02): this server dials each
//! paired server, asks what it shares, and syncs each library into its
//! cache ([`super::cache`]).
//!
//! It syncs on connecting (at start, and each time the connection is made
//! again), then every `federation.sync_interval_secs` (an hour by
//! default, a live setting), and when an admin asks
//! ([`Federation::refresh`]). An origin is online while a connection to
//! it is open; one that cannot be dialled is tried again later, less
//! often each time. A library the origin no longer lists, or refuses as
//! not shared, is dropped from the cache, keeping the items a user here
//! holds, flagged unavailable.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use spoolframe_federation::{DialError, Dialed, Pages};
use spoolframe_protocol::federation::{SharedLibrary, Standing, SyncLibrary, SyncMode, SyncPage};
use spoolframe_types::MediaId;

use super::exposure::WEEK_DAYS;
use super::{Federation, endpoint_addr, parsed};
use crate::store::StoreError;
use crate::store::federation::PeerRecord;
use crate::store::federation_cache::{CacheSynced, FileLimits};
use crate::store::federation_tokens::DAY;

/// How often the replica looks at its pairs: whether each is connected,
/// and due a sync.
const TICK: Duration = Duration::from_secs(15);

/// The first wait before dialling an origin again after it could not be
/// reached; doubled at each failure, to at most [`MAX_RETRY`].
const FIRST_RETRY: Duration = Duration::from_secs(30);
const MAX_RETRY: Duration = Duration::from_secs(30 * 60);

/// The most posters fetched from an origin at one tick, and how many at
/// once (M5): a first sync of a big library is fetched over a few minutes,
/// within the origin's pace, and never holds the sync lock.
const POSTER_SWEEP: u32 = 200;
const POSTER_FETCHES: usize = 6;

/// The default time between two syncs of a pair.
pub const DEFAULT_SYNC_EVERY: Duration = Duration::from_secs(3600);

/// How much one sync, and one origin, may put in the cache (H1): an
/// origin that answers without end, or with more than the settings allow
/// (P8-04), is cut off, and its cache stays as it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncLimits {
    /// Pages in one library's answer.
    pub pages: u64,
    /// Items, and ids of items gone, in one library's answer.
    pub items: u64,
    /// Items of one origin's in the cache, every library together.
    pub origin_items: u64,
}

/// Where a sync's pages come from: a pair's answer, or a test's.
pub(crate) trait PageSource {
    async fn next_page(&mut self) -> Result<Option<SyncPage>, DialError>;
}

impl PageSource for Pages {
    async fn next_page(&mut self) -> Result<Option<SyncPage>, DialError> {
        self.next().await
    }
}

/// Why syncing a pair did not go through.
#[derive(Debug, thiserror::Error)]
pub enum ReplicaError {
    #[error("no paired server {0:?}")]
    NoSuchPeer(String),
    /// The other server could not be reached, or stopped answering.
    #[error(transparent)]
    Dial(#[from] DialError),
    /// It does not pair with this server any more, or holds this one's
    /// new key.
    #[error("the other server does not pair with this one now ({0:?})")]
    NotPaired(Standing),
    /// A library's answer went past a limit ([`SyncLimits`]): it is not
    /// kept.
    #[error("the answer has more {what} than the {limit} kept")]
    TooLarge { what: &'static str, limit: u64 },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// What the replica keeps of each pair between ticks.
#[derive(Debug)]
struct Tended {
    last_sync: Option<Instant>,
    retry_at: Instant,
    failures: u32,
}

impl Default for Tended {
    fn default() -> Self {
        Self {
            last_sync: None,
            retry_at: Instant::now(),
            failures: 0,
        }
    }
}

fn retry_after(failures: u32) -> Duration {
    FIRST_RETRY
        .saturating_mul(1 << failures.saturating_sub(1).min(16))
        .min(MAX_RETRY)
}

impl Federation {
    /// Start replicating from the pairs, once the endpoint is up: until it
    /// closes, or the federation is dropped.
    pub(crate) fn start_replica(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tended: HashMap<String, Tended> = HashMap::new();
            loop {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if this
                    .attached
                    .get()
                    .is_none_or(|attached| attached.endpoint.is_closed())
                {
                    return;
                }
                this.tend_all(&mut tended).await;
                let wake = this.wake.clone();
                drop(this);
                tokio::select! {
                    () = tokio::time::sleep(TICK) => {}
                    () = wake.notified() => {}
                }
            }
        });
    }

    /// Look at each pair once: dial it if it is not connected (and its
    /// wait is over), and sync it on connecting or when it is due.
    async fn tend_all(&self, tended: &mut HashMap<String, Tended>) {
        // The play tokens past their expiry (P8-03), whoever minted them.
        if let Err(e) = self.store().prune_federation_tokens().await {
            tracing::warn!(error = %e, "cannot prune the expired play tokens");
        }
        // The days' play counts past the week the Federation page shows.
        let today = self.store().now().div_euclid(DAY);
        if let Err(e) = self
            .store()
            .prune_federation_play_days(today - (WEEK_DAYS - 1))
            .await
        {
            tracing::warn!(error = %e, "cannot prune the days' play counts");
        }
        // The walks past their reuse (H2), of libraries no pair asks for
        // again: a whole library's items each.
        self.answering.prune_walks(self.limits().walk_reuse);
        let peers: Vec<PeerRecord> = match self.store().federation_peers().await {
            Ok(peers) => peers.into_iter().filter(PeerRecord::live).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "cannot list the pairs to sync");
                return;
            }
        };
        tended.retain(|id, _| peers.iter().any(|p| &p.id == id));
        for peer in &peers {
            let state = tended.entry(peer.id.clone()).or_default();
            self.tend(peer, state).await;
            // When it was last online, for "offline since" once it is not.
            if self.connected(&peer.endpoint_id)
                && let Err(e) = self.store().peer_seen(&peer.id).await
            {
                tracing::warn!(error = %e, "cannot keep when a paired server was seen");
            }
        }
        // Outside the sync lock: images never hold a sync up.
        self.cache.evict().await;
        for peer in &peers {
            self.sweep_posters(&peer.server_id).await;
        }
    }

    /// Fetch some of `origin`'s posters that have no file yet, a few at
    /// a time; when it says to wait, the rest wait for the next tick. One
    /// that fails is not asked for again until the origin's next sync.
    async fn sweep_posters(&self, origin: &str) {
        let Some(dialed) = self.links.get(origin) else {
            return;
        };
        let posters = match self.cache.missing_posters(origin, POSTER_SWEEP).await {
            Ok(posters) => posters,
            Err(e) => {
                tracing::warn!(origin, error = %e, "cannot list the posters to fetch");
                return;
            }
        };
        let mut fetched = stream::iter(posters)
            .map(|poster| {
                let dialed = dialed.clone();
                async move {
                    let fetched = self.cache.fetch(origin, &dialed, &poster).await;
                    (poster, fetched)
                }
            })
            .buffer_unordered(POSTER_FETCHES);
        while let Some((poster, fetched)) = fetched.next().await {
            match fetched {
                Ok(_) => {}
                Err(DialError::Unavailable) => return,
                Err(e) => {
                    tracing::debug!(origin, error = %e, "a paired server's poster was not fetched");
                    self.cache.missed(origin, poster);
                }
            }
        }
    }

    async fn tend(&self, peer: &PeerRecord, state: &mut Tended) {
        let _syncing = self.syncing.lock(&peer.server_id).await;
        let now = Instant::now();
        let connected = self.links.get(&peer.server_id).is_some();
        if !connected {
            if now < state.retry_at {
                return;
            }
            if let Err(e) = self.dial(peer).await {
                state.failures += 1;
                let wait = retry_after(state.failures);
                state.retry_at = now + wait;
                tracing::info!(
                    peer = %peer.endpoint_id,
                    name = %peer.display_name,
                    error = %e,
                    retry_in_secs = wait.as_secs(),
                    "a paired server is offline"
                );
                return;
            }
            state.failures = 0;
        }
        let due = state
            .last_sync
            .is_none_or(|at| now.duration_since(at) >= self.sync_every());
        if !connected || due {
            match self.sync_peer(peer).await {
                Ok(()) => state.last_sync = Some(now),
                Err(e) => tracing::warn!(
                    peer = %peer.endpoint_id,
                    name = %peer.display_name,
                    error = %e,
                    "cannot sync a paired server"
                ),
            }
        }
    }

    /// Dial `peer` and keep the connection: it is online. One that does
    /// not pair with this server any more shares nothing: its cached
    /// libraries go.
    async fn dial(&self, peer: &PeerRecord) -> Result<(), ReplicaError> {
        let attached = self
            .attached
            .get()
            .ok_or_else(|| DialError::Unreachable("this server is starting".into()))?;
        let endpoint: iroh::EndpointId = peer
            .endpoint_id
            .parse()
            .map_err(|_| DialError::Unreachable("a pair's key does not read".into()))?;
        let addr = endpoint_addr(endpoint, parsed(&peer.addrs), &peer.relays);
        let (dialed, info) = Dialed::connect(&attached.endpoint, addr, self.hello()).await?;
        if info.standing != Standing::Paired {
            dialed.close();
            if info.standing == Standing::Stranger {
                tracing::warn!(
                    peer = %peer.endpoint_id,
                    name = %peer.display_name,
                    "a paired server no longer pairs with this one: what it shared is dropped"
                );
                self.drop_cached(&peer.server_id, None).await?;
            }
            return Err(ReplicaError::NotPaired(info.standing));
        }
        tracing::info!(
            peer = %peer.endpoint_id,
            name = %peer.display_name,
            "connected to a paired server"
        );
        self.links.set(&peer.server_id, Arc::new(dialed));
        Ok(())
    }

    /// Sync every library `peer` shares with this one, over the
    /// connection held to it: libraries it no longer lists go, each other
    /// is synced from its cursor, and the posters of the items that came
    /// are fetched.
    async fn sync_peer(&self, peer: &PeerRecord) -> Result<(), ReplicaError> {
        let origin = peer.server_id.as_str();
        let dialed = self
            .links
            .get(origin)
            .ok_or_else(|| DialError::Unreachable("not connected".into()))?;
        let listed = match dialed.libraries().await {
            Ok(listed) => listed,
            Err(e) => {
                self.links.drop_origin(origin);
                return Err(e.into());
            }
        };
        let listed: Vec<(SharedLibrary, &str)> = listed
            .into_iter()
            .filter_map(|l| {
                // One this build does not know is not cached.
                let kind = l.kind.as_str()?;
                Some((l, kind))
            })
            .collect();
        let cached = self.store().cached_libraries(Some(origin)).await?;
        // A library shared since the last sync: the clients list it.
        if listed
            .iter()
            .any(|(l, _)| !cached.iter().any(|c| c.library == l.id))
        {
            self.links.told();
        }
        for cached in cached {
            if !listed.iter().any(|(l, _)| l.id == cached.library) {
                tracing::info!(
                    peer = %peer.endpoint_id,
                    library = %cached.library,
                    title = %cached.title,
                    "a paired server no longer shares a library: its cache is dropped"
                );
                self.drop_cached(origin, Some(&cached.library)).await?;
                self.store()
                    .withdraw_cached(origin, &peer.id, &cached.library, &cached.title)
                    .await?;
            }
        }
        self.cache.sync_began(origin);
        for (library, kind) in &listed {
            self.store()
                .cache_library(origin, &peer.id, library, kind)
                .await?;
            match self.sync_library(peer, &dialed, &library.id).await {
                Ok(()) => {}
                Err(ReplicaError::Dial(DialError::NotShared)) => {
                    self.drop_cached(origin, Some(&library.id)).await?;
                    self.store()
                        .withdraw_cached(origin, &peer.id, &library.id, &library.title)
                        .await?;
                }
                Err(ReplicaError::Dial(DialError::Unavailable)) => tracing::info!(
                    peer = %peer.endpoint_id,
                    library = %library.id,
                    "a paired server cannot answer for a library now: its cache is kept"
                ),
                Err(e @ ReplicaError::TooLarge { .. }) => tracing::warn!(
                    peer = %peer.endpoint_id,
                    library = %library.id,
                    error = %e,
                    "a paired server's library is past the limits: its cache is kept as it was"
                ),
                Err(e) => {
                    if matches!(e, ReplicaError::Dial(DialError::Broken(_))) {
                        self.links.drop_origin(origin);
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Sync `peer`'s `library` from its cursor.
    async fn sync_library(
        &self,
        peer: &PeerRecord,
        dialed: &Dialed,
        library: &str,
    ) -> Result<(), ReplicaError> {
        let origin = peer.server_id.as_str();
        let since = self
            .store()
            .cached_library(origin, library)
            .await?
            .and_then(|l| l.cursor);
        let mut pages = dialed
            .sync(SyncLibrary {
                library: library.to_owned(),
                since,
            })
            .await?;
        self.cache_sync(origin, &peer.id, library, &mut pages).await
    }

    /// Read `pages`, a sync of `origin`'s `library` over pair `peer`, into
    /// the cache: each
    /// page staged as it comes, and all written, with the cursor, once the
    /// last is (H1). An answer cut short, or past the limits as the sync
    /// begins, writes nothing.
    pub(crate) async fn cache_sync(
        &self,
        origin: &str,
        peer: &str,
        library: &str,
        pages: &mut impl PageSource,
    ) -> Result<(), ReplicaError> {
        let limits = self.limits().sync;
        let store = self.store();
        store.begin_cache_sync(origin, library).await?;
        let read = async {
            let (mut read, mut items) = (0u64, 0u64);
            let mut whole = None;
            while let Some(page) = pages.next_page().await? {
                if page.mode == SyncMode::Unknown {
                    return Err(DialError::OutOfTurn.into());
                }
                read += 1;
                items += (page.items.len() + page.removed.len()) as u64;
                if read > limits.pages {
                    return Err(ReplicaError::TooLarge {
                        what: "pages",
                        limit: limits.pages,
                    });
                }
                if items > limits.items {
                    return Err(ReplicaError::TooLarge {
                        what: "items",
                        limit: limits.items,
                    });
                }
                store
                    .stage_cache_page(origin, library, &page.items, &page.removed)
                    .await?;
                whole = Some((page.mode, page.cursor));
            }
            whole.ok_or_else(|| DialError::OutOfTurn.into())
        };
        let (mode, cursor) = match read.await {
            Ok(whole) => whole,
            Err(e) => {
                if let Err(e) = store.abort_cache_sync(origin, library).await {
                    tracing::warn!(origin, library, error = %e, "cannot drop a sync's pages");
                }
                return Err(e);
            }
        };
        match store
            .finish_cache_sync(
                origin,
                peer,
                library,
                &cursor,
                mode == SyncMode::Snapshot,
                limits.origin_items,
            )
            .await?
        {
            CacheSynced::Written => {}
            CacheSynced::OverLimit { .. } => {
                return Err(ReplicaError::TooLarge {
                    what: "items cached of the server",
                    limit: limits.origin_items,
                });
            }
        };
        let gone = store.forget_cached_files(origin, false).await?;
        self.cache.delete_files(origin, &gone).await;
        tracing::debug!(
            origin,
            library,
            ?mode,
            "a paired server's library is synced"
        );
        Ok(())
    }

    /// Drop `origin`'s cached `library`, or all of it with `None`: the
    /// items a user here holds are kept, unavailable and without artwork.
    pub(crate) async fn drop_cached(
        &self,
        origin: &str,
        library: Option<&str>,
    ) -> Result<(), StoreError> {
        let keep: HashSet<String> = self
            .store()
            .held_federated_ids()
            .await?
            .iter()
            .filter_map(|id| id.parse::<MediaId>().ok())
            .filter_map(|id| {
                let (of, remote) = id.remote()?;
                (of == origin).then(|| remote.to_string())
            })
            .collect();
        self.store().drop_cached(origin, library, &keep).await?;
        let gone = self
            .store()
            .forget_cached_files(origin, library.is_none())
            .await?;
        self.cache.delete_files(origin, &gone).await;
        if library.is_none() {
            self.cache.delete_origin(origin).await;
        }
        // The clients list one library fewer (P8-02).
        self.links.told();
        Ok(())
    }

    /// The pair with `origin` is gone: its connection is closed, and its
    /// cache dropped but for what a user here holds.
    pub(crate) async fn forget_origin(&self, origin: &str) {
        self.links.drop_origin(origin);
        // After the origin's sync in flight, if any: one that wrote its
        // pages after the drop would bring the rows back.
        let _syncing = self.syncing.lock(origin).await;
        if let Err(e) = self.drop_cached(origin, None).await {
            tracing::error!(origin, error = %e, "cannot drop an unpaired server's cache");
        }
    }

    /// Sync pair `id` now, as an admin's Refresh does: dial it if it is
    /// not connected, then sync every library it shares.
    pub async fn refresh(&self, id: &str) -> Result<(), ReplicaError> {
        let peer = self
            .store()
            .federation_peer(id)
            .await?
            .filter(PeerRecord::live)
            .ok_or_else(|| ReplicaError::NoSuchPeer(id.to_owned()))?;
        let _syncing = self.syncing.lock(&peer.server_id).await;
        if self.links.get(&peer.server_id).is_none() {
            self.dial(&peer).await?;
        }
        self.sync_peer(&peer).await
    }

    /// Whether the paired server whose server id is `origin` is online:
    /// this server holds a connection to it.
    pub fn online(&self, origin: &str) -> bool {
        self.links.online(origin)
    }

    /// The time between two syncs of a pair now.
    fn sync_every(&self) -> Duration {
        *self.sync_every.lock()
    }

    /// New replica settings: a live setting. The next tick uses the new
    /// interval; files past a lower limit go at the next tick.
    pub fn set_replica(&self, sync_every: Duration, files: FileLimits) {
        *self.sync_every.lock() = sync_every;
        self.cache.set_limits(files);
        self.wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use spoolframe_protocol::federation::{RemoteItem, RemoteKind, RemoteLibraryKind};

    use super::*;
    use crate::auth::Auth;
    use crate::network::ServerName;
    use crate::store::federation::NewPeer;
    use crate::store::tests::test_store;
    use crate::test_support::FakeCatalog;

    const ORIGIN: &str = "aaaaaaaaaaaaaaaa";

    /// A paired server's answer: `pages`, then, with `endless`, a page
    /// saying there is more for ever.
    struct Answer {
        pages: std::vec::IntoIter<SyncPage>,
        endless: bool,
    }

    impl PageSource for Answer {
        async fn next_page(&mut self) -> Result<Option<SyncPage>, DialError> {
            Ok(self
                .pages
                .next()
                .or_else(|| self.endless.then(|| page(SyncMode::Changes, &[], true))))
        }
    }

    fn page(mode: SyncMode, films: &[&str], more: bool) -> SyncPage {
        SyncPage {
            mode,
            cursor: "c".into(),
            items: films
                .iter()
                .map(|key| {
                    RemoteItem {
                        id: format!("plex:movie:{key}"),
                        kind: RemoteKind::Movie,
                        title: format!("Film {key}"),
                        parent: None,
                        grandparent: None,
                        parent_title: None,
                        grandparent_title: None,
                        index: None,
                        parent_index: None,
                        year: None,
                        date: None,
                        summary: None,
                        content_rating: None,
                        duration_ms: None,
                        count: None,
                        genres: Vec::new(),
                        artwork: Vec::new(),
                        external_ids: Vec::new(),
                        hash: String::new(),
                    }
                    .hashed()
                })
                .collect(),
            removed: Vec::new(),
            more,
        }
    }

    /// Bob's server, paired with Alice's (ORIGIN), whose Movies it caches.
    async fn replica(test: &str) -> (crate::store::tests::TestStore, Federation, String) {
        let store = test_store(test).await;
        let federation = Federation::new(
            Auth::new(store.store.clone()),
            Arc::new(FakeCatalog::new("/nowhere/movie.mkv".into())),
            "bbbbbbbbbbbbbbbb".parse().unwrap(),
            ServerName::new("Bob's server"),
            super::super::ReplicaSettings::in_dir("/nowhere/federation".into()),
        );
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
        store
            .cache_library(
                ORIGIN,
                &peer.id,
                &SharedLibrary {
                    id: "plex:1".into(),
                    title: "Movies".into(),
                    kind: RemoteLibraryKind::Movies,
                },
                "movies",
            )
            .await
            .unwrap();
        (store, federation, peer.id)
    }

    async fn sync(
        (federation, peer): (&Federation, &str),
        pages: Vec<SyncPage>,
        endless: bool,
        limits: SyncLimits,
    ) -> Result<(), ReplicaError> {
        let mut answer = Answer {
            pages: pages.into_iter(),
            endless,
        };
        federation.set_limits(crate::federation::Limits {
            sync: limits,
            ..crate::federation::Limits::default()
        });
        federation
            .cache_sync(ORIGIN, peer, "plex:1", &mut answer)
            .await
    }

    async fn cached(federation: &Federation) -> u32 {
        federation
            .store()
            .cached_page(ORIGIN, "plex:1", "movie", 0, 100)
            .await
            .unwrap()
            .1
    }

    /// The pair goes while a sync of it is in flight: the cache is dropped
    /// once the sync is done, so what it wrote does not outlive the pair.
    #[tokio::test]
    async fn an_unpairing_waits_for_the_sync_in_flight_then_drops_it() {
        let (_store, federation, peer) = replica("fed-replica-forget").await;
        let syncing = federation.syncing.lock(ORIGIN).await;
        let forget = federation.forget_origin(ORIGIN);
        tokio::pin!(forget);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut forget)
                .await
                .is_err(),
            "waits for the sync"
        );
        let whole = vec![page(SyncMode::Snapshot, &["1", "2"], false)];
        sync(
            (&federation, &peer),
            whole,
            false,
            crate::federation::Limits::default().sync,
        )
        .await
        .unwrap();
        drop(syncing);
        forget.await;
        assert_eq!(cached(&federation).await, 0);
    }

    /// An unpaired server's rows, not dropped yet, are found by no search
    /// and listed in no Recently added.
    #[tokio::test]
    async fn an_unpaired_server_s_rows_are_not_searched_or_listed() {
        let (store, federation, peer) = replica("fed-replica-live-only").await;
        let whole = vec![page(SyncMode::Snapshot, &["1", "2"], false)];
        sync(
            (&federation, &peer),
            whole,
            false,
            crate::federation::Limits::default().sync,
        )
        .await
        .unwrap();
        let found = || async {
            let (_, total) = store
                .search_cached("movie", "Film", None, None, 10)
                .await
                .unwrap();
            let recent = store.recent_cached(None, 10).await.unwrap().len();
            (total, recent)
        };
        assert_eq!(found().await, (2, 2));
        store.revoke_peer(&peer).await.unwrap().unwrap();
        assert_eq!(found().await, (0, 0));
    }

    /// An origin that says there is more for ever is cut off at the page
    /// limit, and one that sends more than the limits allow is not kept:
    /// the cache stays as the last whole sync left it (H1).
    #[tokio::test]
    async fn an_answer_past_the_limits_is_cut_off_and_the_cache_kept() {
        let (_store, federation, peer) = replica("fed-replica-limits").await;
        let pair = (&federation, peer.as_str());
        let whole = vec![page(SyncMode::Snapshot, &["1", "2"], false)];
        sync(
            pair,
            whole,
            false,
            crate::federation::Limits::default().sync,
        )
        .await
        .unwrap();
        assert_eq!(cached(&federation).await, 2);

        let endless = vec![page(SyncMode::Changes, &["3"], true)];
        let cut = sync(
            pair,
            endless,
            true,
            crate::federation::Limits::default().sync,
        )
        .await;
        assert!(
            matches!(cut, Err(ReplicaError::TooLarge { what: "pages", .. })),
            "{cut:?}"
        );
        assert_eq!(cached(&federation).await, 2, "nothing of it kept");

        let few = SyncLimits {
            pages: 10,
            items: 2,
            origin_items: 3,
        };
        let many = vec![page(SyncMode::Changes, &["3", "4", "5"], false)];
        let cut = sync(pair, many, false, few).await;
        assert!(
            matches!(cut, Err(ReplicaError::TooLarge { what: "items", .. })),
            "{cut:?}"
        );
        let past_the_origin = vec![
            page(SyncMode::Changes, &["3"], true),
            page(SyncMode::Changes, &["4"], false),
        ];
        let cut = sync(pair, past_the_origin, false, few).await;
        assert!(
            matches!(
                cut,
                Err(ReplicaError::TooLarge {
                    what: "items cached of the server",
                    ..
                })
            ),
            "{cut:?}"
        );
        assert_eq!(cached(&federation).await, 2);
        let cursor = federation
            .store()
            .cached_library(ORIGIN, "plex:1")
            .await
            .unwrap()
            .unwrap()
            .cursor;
        assert_eq!(cursor.as_deref(), Some("c"), "the last whole sync's");
    }

    /// A very large library (the 2026-10-23 decision): 300,000 tracks are
    /// past the default limits and kept out, and sync once the settings
    /// raise them.
    #[tokio::test]
    async fn a_300k_item_library_syncs_once_the_limits_are_raised() {
        let (_store, federation, peer) = replica("fed-replica-300k").await;
        let pair = (&federation, peer.as_str());
        let keys: Vec<String> = (0..300_000).map(|i| i.to_string()).collect();
        let pages = || {
            let chunks: Vec<&[String]> = keys.chunks(1_000).collect();
            let last = chunks.len() - 1;
            chunks
                .iter()
                .enumerate()
                .map(|(i, keys)| {
                    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
                    page(SyncMode::Snapshot, &keys, i != last)
                })
                .collect::<Vec<_>>()
        };
        let defaults = crate::federation::Limits::default().sync;
        let cut = sync(pair, pages(), false, defaults).await;
        assert!(
            matches!(cut, Err(ReplicaError::TooLarge { what: "items", .. })),
            "{cut:?}"
        );
        assert_eq!(cached(&federation).await, 0);

        let raised = crate::config::FederationConfig {
            sync_max_items: Some(300_000),
            max_cached_items: Some(300_000),
            ..crate::config::FederationConfig::default()
        }
        .limits()
        .sync;
        sync(pair, pages(), false, raised).await.unwrap();
        assert_eq!(cached(&federation).await, 300_000);
    }

    #[test]
    fn an_offline_server_is_tried_less_often_each_time_to_a_limit() {
        assert_eq!(retry_after(1), FIRST_RETRY);
        assert_eq!(retry_after(2), FIRST_RETRY * 2);
        assert_eq!(retry_after(40), MAX_RETRY);
    }
}
