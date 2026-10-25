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
use super::{Federation, peer_addr};
use crate::store::StoreError;
use crate::store::federation::PeerRecord;
use crate::store::federation_cache::{CacheSynced, FileLimits, OriginLimits};
use crate::store::federation_tokens::DAY;

/// How often the replica looks at its pairs: whether each is connected,
/// and due a sync.
const TICK: Duration = Duration::from_secs(15);

/// The first wait before dialling an origin again after it could not be
/// reached; doubled at each failure, to at most [`MAX_RETRY`].
pub(super) const FIRST_RETRY: Duration = Duration::from_secs(30);
pub(super) const MAX_RETRY: Duration = Duration::from_secs(30 * 60);

/// The most posters fetched from an origin at one tick, and how many at
/// once (M5): a first sync of a big library is fetched over a few minutes,
/// within the origin's pace, and never holds the sync lock.
const POSTER_SWEEP: u32 = 200;
const POSTER_FETCHES: usize = 6;

/// How long one pair's poster sweep may take, its fetches together: the
/// next tick fetches the rest.
const SWEEP_WAIT: Duration = Duration::from_secs(5 * 60);

/// How many pairs are dialled and synced at once: one that stops
/// answering holds its own place, not the others' (C8-05).
const PAIRS_AT_ONCE: usize = 4;

/// How long one pair's sync may take, every library together (C8-05):
/// each page comes within its own wait, but 2,000 of them, each just
/// inside it, would hold the pair's place for more than a day. The
/// dialled side's bound on sending an answer is the same constant.
pub const SYNC_BUDGET: Duration = spoolframe_federation::SYNC_WAIT;

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
    /// Bytes of one library's answer, its items as the cache keeps them
    /// (JSON, every character escaped as it is there): counted as each
    /// page is staged, so one past it ends the sync at that page (C8-02).
    pub bytes: u64,
    /// Bytes of one origin's items in the cache, every library together.
    pub origin_bytes: u64,
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
    /// The pair's key answered under another server id than the pair's
    /// (C8-06): refused, as an incoming hello is.
    #[error("the paired server's key answered as another server, {0:?}")]
    OtherServer(String),
    /// The pair moved to another key, or ended, while it was dialled or
    /// synced (C8-01): what came from the old key is not kept.
    #[error("the pair moved to another key, or ended, meanwhile")]
    Moved,
    /// A library's answer went past a limit ([`SyncLimits`]): it is not
    /// kept.
    #[error("the answer has more {what} than the {limit} kept")]
    TooLarge { what: &'static str, limit: u64 },
    /// The sync took longer than [`SYNC_BUDGET`]: what it read of the
    /// library it was on is not kept.
    #[error("the sync took longer than its {0:?}")]
    TooSlow(Duration),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// What the replica keeps of each pair between ticks: of the key it was
/// last tended at, as a new key starts afresh. Its times are the server's
/// [`Clock`](crate::clock::Clock)'s, read as each attempt ends, not as it
/// began (X8-05): an attempt that fails after its wait's worth of time
/// would otherwise have used its wait up before it ended, and be made
/// again at the next tick.
#[derive(Debug)]
pub(super) struct Tended {
    /// The key, hex.
    pub(super) endpoint: String,
    pub(super) last_sync: Option<Instant>,
    /// No dial nor sync before then: one failed, or the pair could not be
    /// reached.
    pub(super) retry_at: Instant,
    pub(super) failures: u32,
}

impl Tended {
    /// Nothing tended yet, as of `now`: due at once.
    pub(super) fn at(now: Instant) -> Self {
        Self {
            endpoint: String::new(),
            last_sync: None,
            retry_at: now,
            failures: 0,
        }
    }

    /// Another failure, ending at `now`: no dial nor sync for a while
    /// from then, longer each time.
    fn failed(&mut self, now: Instant) -> Duration {
        self.failures += 1;
        let wait = retry_after(self.failures);
        self.retry_at = now + wait;
        wait
    }
}

fn retry_after(failures: u32) -> Duration {
    FIRST_RETRY
        .saturating_mul(1 << failures.saturating_sub(1).min(16))
        .min(MAX_RETRY)
}

impl Federation {
    /// Start replicating from the pairs, once the endpoint is up: until it
    /// closes, or the federation is dropped. The upkeep runs on its own,
    /// so a pair that stops answering never holds it up (C8-05).
    pub(crate) fn start_replica(self: &Arc<Self>) {
        self.spawn_upkeep();
        self.spawn_tending();
    }

    /// The upkeep, each tick.
    fn spawn_upkeep(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if !this.endpoint_open() {
                    return;
                }
                this.upkeep().await;
                drop(this);
                tokio::time::sleep(TICK).await;
            }
        });
    }

    /// The pairs, each tick, and when woken.
    fn spawn_tending(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tended = HashMap::new();
            let places = Arc::new(tokio::sync::Semaphore::new(PAIRS_AT_ONCE));
            loop {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if !this.endpoint_open() {
                    return;
                }
                this.tend_all(&mut tended, &places).await;
                let wake = this.wake.clone();
                drop(this);
                tokio::select! {
                    () = tokio::time::sleep(TICK) => {}
                    () = wake.notified() => {}
                }
            }
        });
    }

    /// Whether this server's endpoint is up, and not closed since.
    fn endpoint_open(&self) -> bool {
        self.attached
            .get()
            .is_some_and(|attached| !attached.endpoint.is_closed())
    }

    /// What the replica keeps tidy each tick, whoever answers: what has
    /// expired, and the artwork past its caps.
    async fn upkeep(&self) {
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
        // again: a whole library's items each; and the pairs' buckets full
        // again.
        self.answering.prune_walks(self.limits().walk_reuse);
        self.answering.prune_asks(std::time::Instant::now());
        // Downloads of the pairs' items that ended a while ago (P8-07), and
        // the sessions for reads relayed here that no read uses now.
        if let Err(e) = self
            .store()
            .prune_remote_downloads(super::downloads::ENDED_KEPT)
            .await
        {
            tracing::warn!(error = %e, "cannot forget the ended downloads of the pairs' items");
        }
        self.end_idle_sessions().await;
        // Outside the sync lock: images never hold a sync up.
        self.cache.evict().await;
        self.cache.sweep_orphans_if_due().await;
    }

    /// Look at each pair: dial it if it is not connected (and its wait is
    /// over), and sync it on connecting or when it is due, then fetch some
    /// of its posters. Each pair on its own, [`PAIRS_AT_ONCE`] at once; one
    /// still at it from an earlier tick is left to it.
    async fn tend_all(
        self: &Arc<Self>,
        tended: &mut HashMap<String, Arc<tokio::sync::Mutex<Tended>>>,
        places: &Arc<tokio::sync::Semaphore>,
    ) {
        let peers: Vec<PeerRecord> = match self.store().federation_peers().await {
            Ok(peers) => peers.into_iter().filter(PeerRecord::live).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "cannot list the pairs to sync");
                return;
            }
        };
        tended.retain(|id, _| peers.iter().any(|p| &p.id == id));
        let now = self.store().clock().now();
        for peer in peers {
            let state = tended
                .entry(peer.id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(Tended::at(now))))
                .clone();
            let Ok(mut state) = state.try_lock_owned() else {
                continue;
            };
            let (this, places) = (self.clone(), places.clone());
            tokio::spawn(async move {
                let Ok(place) = places.acquire_owned().await else {
                    return;
                };
                this.tend(&peer, &mut state).await;
                // When it was last online, for "offline since" once it is not.
                if this.connected(&peer.endpoint_id)
                    && let Err(e) = this.store().peer_seen(&peer.id).await
                {
                    tracing::warn!(error = %e, "cannot keep when a paired server was seen");
                }
                // The place is for dials and syncs: the sweep, up to
                // SWEEP_WAIT, must not hold one from the other pairs. It
                // is still one at a time per pair, by `state`, held to the
                // end of the task.
                drop(place);
                // Within a while: the rest come at the next tick.
                let _ = tokio::time::timeout(SWEEP_WAIT, this.sweep_posters(&peer)).await;
            });
        }
    }

    /// Fetch some of `peer`'s posters that have no file yet, a few at
    /// a time; when it says to wait, the rest wait for the next tick. One
    /// that fails, `unavailable` too, waits before it is asked for again,
    /// longer each time ([`FIRST_POSTER_RETRY`](super::cache::FIRST_POSTER_RETRY)
    /// doubled, to [`MAX_POSTER_RETRY`](super::cache::MAX_POSTER_RETRY)),
    /// and is asked for after the origin's next sync.
    async fn sweep_posters(&self, peer: &PeerRecord) {
        #[cfg(test)]
        self.holds.sweeping.here().await;
        let origin = peer.server_id.as_str();
        let Some(dialed) = self.links.pinned(peer) else {
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
            let Err(e) = fetched else {
                continue;
            };
            // `unavailable` is the origin past the pair's pace, or failing
            // this image (its PMS could not fetch it): one the origin
            // cannot answer would otherwise be asked for at every tick.
            let wait = self.cache.missed(origin, poster);
            tracing::debug!(
                origin,
                error = %e,
                retry_in_secs = wait.as_secs(),
                "a paired server's poster was not fetched"
            );
            if matches!(e, DialError::Unavailable) {
                return;
            }
        }
    }

    /// Tend pair `listed`, as the list of pairs gave it, once: dial it if
    /// it is not connected, and sync it on connecting or when it is due,
    /// unless its last dial or sync failed a while ago. A key confirmed
    /// since the last time is dialled at once, whatever the old one's
    /// failures (C8-01).
    pub(super) async fn tend(&self, listed: &PeerRecord, state: &mut Tended) {
        // Read again: a key confirmed since the list was read is dialled
        // now, not the old one (C8-01). Not under the sync lock, which
        // only the sync takes (C8-05).
        let peer = match self.pair_now(listed).await {
            Ok(Some(peer)) => peer,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(error = %e, "cannot look a pair up to sync it");
                return;
            }
        };
        let peer = &peer;
        let clock = self.store().clock();
        let now = clock.now();
        if state.endpoint != peer.endpoint_id {
            *state = Tended {
                endpoint: peer.endpoint_id.clone(),
                ..Tended::at(now)
            };
        }
        if now < state.retry_at {
            return;
        }
        let connected = self.links.pinned(peer).is_some();
        if !connected {
            // Not under the sync lock: an unpairing waits for a sync in
            // flight, never for a dial (C8-05).
            match self.connect(peer).await {
                Ok(()) => {}
                // Its key moved meanwhile: the new one is dialled at the
                // next tick, not as one that failed.
                Err(ReplicaError::Moved) => return,
                // The wait runs from the failure, not from the attempt's
                // start (X8-05): a dial that fails late waits its whole
                // wait still, and the log says so.
                Err(e) => {
                    let wait = state.failed(clock.now());
                    tracing::info!(
                        peer = %peer.endpoint_id,
                        name = %peer.display_name,
                        error = %e,
                        retry_in_secs = wait.as_secs(),
                        "a paired server is offline"
                    );
                    return;
                }
            }
        }
        let due = state
            .last_sync
            .is_none_or(|at| now.duration_since(at) >= self.sync_every());
        if !connected || due {
            match self.sync_live(peer).await {
                Ok(()) => {
                    state.last_sync = Some(clock.now());
                    state.failures = 0;
                }
                Err(ReplicaError::Moved) => {}
                // A pair whose sync fails waits as one that cannot be
                // dialled does: it does not take a place each tick, and
                // the wait runs from the failure (X8-05).
                Err(e) => {
                    let wait = state.failed(clock.now());
                    tracing::warn!(
                        peer = %peer.endpoint_id,
                        name = %peer.display_name,
                        error = %e,
                        retry_in_secs = wait.as_secs(),
                        "cannot sync a paired server"
                    );
                }
            }
        }
    }

    /// Dial `peer` unless a connection to it is open: one dial at a time,
    /// so the replica's and a refresh's never replace each other's. The
    /// pair is read again under the lock, as the play pool's is: a key
    /// confirmed while another dial held it is the one dialled.
    pub(super) async fn connect(&self, peer: &PeerRecord) -> Result<(), ReplicaError> {
        let _dialing = self.dialing.lock(&peer.server_id).await;
        let peer = &self.pair_now(peer).await?.ok_or(ReplicaError::Moved)?;
        if self.links.pinned(peer).is_some() {
            return Ok(());
        }
        self.dial(peer).await
    }

    /// Dial `peer` and keep the connection: it is online. One that does
    /// not pair with this server any more shares nothing: its cached
    /// libraries go. One that answers under another server id is not the
    /// pair (C8-06): refused, and kept for the Federation page, as an
    /// incoming hello under another id is.
    async fn dial(&self, peer: &PeerRecord) -> Result<(), ReplicaError> {
        let attached = self
            .attached
            .get()
            .ok_or_else(|| DialError::Unreachable("this server is starting".into()))?;
        let addr = peer_addr(self.relay_hosts, peer)
            .ok_or_else(|| DialError::Unreachable("a pair's key does not read".into()))?;
        let (dialed, info) = Dialed::connect(&attached.endpoint, addr, self.hello()).await?;
        if info.server_id != peer.server_id {
            dialed.close();
            self.identity_refused(peer, &info.server_id).await;
            return Err(ReplicaError::OtherServer(info.server_id));
        }
        if info.standing != Standing::Paired {
            dialed.close();
            if info.standing == Standing::Stranger {
                tracing::warn!(
                    peer = %peer.endpoint_id,
                    name = %peer.display_name,
                    "a paired server no longer pairs with this one: what it shared is dropped"
                );
                // Only while the pair is the one that said so: a key
                // confirmed, or another pairing, meanwhile keeps its cache.
                let _syncing = self.syncing.lock(&peer.server_id).await;
                if self.still_pinned(peer).await? {
                    self.drop_cached(&peer.server_id, None).await?;
                }
            }
            return Err(ReplicaError::NotPaired(info.standing));
        }
        #[cfg(test)]
        self.holds.dialing.here().await;
        // Kept, then checked against the pin (C8-01): a key confirmed, or
        // an unpairing, while this dial was on its way is either seen here
        // or commits after the connection is kept, and its cleanup closes
        // it. Checked before keeping, a cleanup between the two would find
        // nothing to close.
        let dialed = Arc::new(dialed);
        self.links.set(&peer.server_id, dialed.clone());
        if !self.still_pinned(peer).await? {
            self.links.forget(&peer.server_id, &dialed);
            return Err(ReplicaError::Moved);
        }
        tracing::info!(
            peer = %peer.endpoint_id,
            name = %peer.display_name,
            "connected to a paired server"
        );
        Ok(())
    }

    /// Sync `peer` under its origin's sync lock, within [`SYNC_BUDGET`], if
    /// it is a live pair still: one unpaired while it was dialled is not
    /// written again, and its connection goes.
    async fn sync_live(&self, peer: &PeerRecord) -> Result<(), ReplicaError> {
        let _syncing = self.syncing.lock(&peer.server_id).await;
        // The pair as it is now, under the lock: what the sync goes by.
        // Gone: its unpairing closes its connection, and one made after,
        // to a server paired again under its id, is not this pair's.
        let Some(now) = self.pair_now(peer).await? else {
            return Err(ReplicaError::NoSuchPeer(peer.id.clone()));
        };
        self.sync_peer(&now, tokio::time::Instant::now() + SYNC_BUDGET)
            .await
    }

    /// Sync every library `peer` shares with this one, over the
    /// connection held to it, by `deadline`: libraries it no longer lists
    /// go, each other is synced from its cursor, and the posters of the
    /// items that came are fetched.
    async fn sync_peer(
        &self,
        peer: &PeerRecord,
        deadline: tokio::time::Instant,
    ) -> Result<(), ReplicaError> {
        let origin = peer.server_id.as_str();
        let dialed = self
            .links
            .pinned(peer)
            .ok_or_else(|| DialError::Unreachable("not connected".into()))?;
        let listed = match dialed.libraries().await {
            Ok(listed) => listed,
            Err(e) => {
                self.links.drop_origin(origin);
                return Err(e.into());
            }
        };
        // What it lists drops and withdraws: only while its key is the
        // pair's, as when the sync began.
        if !self.still_pinned(peer).await? {
            self.links.forget(origin, &dialed);
            return Err(ReplicaError::Moved);
        }
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
            match self
                .sync_library(peer, &dialed, &library.id, deadline)
                .await
            {
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

    /// Sync `peer`'s `library` from its cursor, by `deadline`.
    async fn sync_library(
        &self,
        peer: &PeerRecord,
        dialed: &Dialed,
        library: &str,
        deadline: tokio::time::Instant,
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
        self.cache_sync(peer, library, &mut pages, deadline).await
    }

    /// Read `pages`, a sync of `peer`'s `library` from its key, into the
    /// cache: each page staged as it comes, and all written, with the
    /// cursor, once the last is (H1). An answer cut short, past the limits
    /// as the sync begins, or not whole by `deadline` (C8-05), writes
    /// nothing; nor does one whose pair moved to another key, or ended,
    /// before it is written (C8-01).
    pub(crate) async fn cache_sync(
        &self,
        peer: &PeerRecord,
        library: &str,
        pages: &mut impl PageSource,
        deadline: tokio::time::Instant,
    ) -> Result<(), ReplicaError> {
        let origin = peer.server_id.as_str();
        let limits = self.limits().sync;
        let store = self.store();
        store.begin_cache_sync(origin, library).await?;
        let read = async {
            let (mut read, mut items, mut bytes) = (0u64, 0u64, 0u64);
            let mut whole = None;
            let too_slow = || ReplicaError::TooSlow(SYNC_BUDGET);
            loop {
                // A page that is there at once is still past the deadline.
                if tokio::time::Instant::now() >= deadline {
                    return Err(too_slow());
                }
                let Some(page) = tokio::time::timeout_at(deadline, pages.next_page())
                    .await
                    .map_err(|_| too_slow())??
                else {
                    break;
                };
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
                // An item whose id is not the origin's own is not kept
                // (G8-06).
                let kept: Vec<spoolframe_protocol::federation::RemoteItem> = page
                    .items
                    .into_iter()
                    .filter(super::items::keepable)
                    .collect();
                bytes += store
                    .stage_cache_page(origin, library, &kept, &page.removed)
                    .await?;
                if bytes > limits.bytes {
                    return Err(ReplicaError::TooLarge {
                        what: "bytes of metadata",
                        limit: limits.bytes,
                    });
                }
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
        #[cfg(test)]
        self.holds.committing.here().await;
        match store
            .finish_cache_sync(
                origin,
                (&peer.id, &peer.endpoint_id),
                library,
                &cursor,
                mode == SyncMode::Snapshot,
                OriginLimits {
                    items: limits.origin_items,
                    bytes: limits.origin_bytes,
                },
            )
            .await?
        {
            CacheSynced::Written => {}
            CacheSynced::Moved => return Err(ReplicaError::Moved),
            CacheSynced::OverLimit { items, .. } if items > limits.origin_items => {
                return Err(ReplicaError::TooLarge {
                    what: "items cached of the server",
                    limit: limits.origin_items,
                });
            }
            CacheSynced::OverLimit { .. } => {
                return Err(ReplicaError::TooLarge {
                    what: "bytes of metadata cached of the server",
                    limit: limits.origin_bytes,
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
        // Its downloads here stay, no longer available (P8-07).
        self.downloads_unavailable(origin, library).await;
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
        self.connect(&peer).await?;
        self.sync_live(&peer).await
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
        let peer = federation
            .store()
            .federation_peer(peer)
            .await
            .unwrap()
            .unwrap();
        federation
            .cache_sync(&peer, "plex:1", &mut answer, in_budget())
            .await
    }

    async fn record_of(federation: &Federation, peer: &str) -> PeerRecord {
        federation
            .store()
            .federation_peer(peer)
            .await
            .unwrap()
            .unwrap()
    }

    fn in_budget() -> tokio::time::Instant {
        tokio::time::Instant::now() + SYNC_BUDGET
    }

    async fn cached(federation: &Federation) -> u32 {
        federation
            .store()
            .cached_page(ORIGIN, "plex:1", "movie", 0, 100)
            .await
            .unwrap()
            .1
    }

    /// A pair's poster sweep, which may take minutes, does not hold the
    /// pair's place: the other pairs are dialled and synced meanwhile.
    #[tokio::test]
    async fn a_poster_sweep_does_not_hold_a_place() {
        let (_store, federation, _peer) = replica("fed-replica-sweep-place").await;
        let federation = Arc::new(federation);
        let places = Arc::new(tokio::sync::Semaphore::new(1));
        let (swept, go) = federation.holds.sweeping.next();
        federation.tend_all(&mut HashMap::new(), &places).await;
        tokio::time::timeout(Duration::from_secs(10), swept)
            .await
            .expect("the sweep starts")
            .unwrap();
        assert_eq!(places.available_permits(), 1, "the sweep holds the place");
        go.send(()).unwrap();
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

    /// The audit's C8-01: a sync read from the pair's old key, written
    /// after a new key is confirmed, writes nothing. The cache and the
    /// cursor stay as they were.
    #[tokio::test]
    async fn a_sync_of_the_old_key_written_after_a_new_one_is_confirmed_is_not_kept() {
        let (store, federation, peer) = replica("fed-replica-moved").await;
        let record = store.federation_peer(&peer).await.unwrap().unwrap();
        let new = iroh::SecretKey::from_bytes(&[12; 32]).public().to_string();
        store.hold_peer_key(&peer, &new).await.unwrap();
        let mut answer = Answer {
            pages: vec![page(SyncMode::Snapshot, &["1", "2"], false)].into_iter(),
            endless: false,
        };
        let (held, go) = federation.holds.committing.next();
        let confirm = async {
            held.await.unwrap();
            federation.confirm_key(&peer, &new, "bob").await.unwrap();
            go.send(()).unwrap();
        };
        let (synced, ()) = tokio::join!(
            federation.cache_sync(&record, "plex:1", &mut answer, in_budget()),
            confirm
        );
        assert!(matches!(synced, Err(ReplicaError::Moved)), "{synced:?}");
        assert_eq!(cached(&federation).await, 0);
        let cursor = store
            .cached_library(ORIGIN, "plex:1")
            .await
            .unwrap()
            .unwrap()
            .cursor;
        assert_eq!(cursor, None);
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
            ..crate::federation::Limits::default().sync
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

    /// A paired server's answer for ever, each page a while after the
    /// last: well within a page's own wait.
    struct Slow {
        every: Duration,
        served: usize,
    }

    impl PageSource for Slow {
        async fn next_page(&mut self) -> Result<Option<SyncPage>, DialError> {
            tokio::time::sleep(self.every).await;
            self.served += 1;
            let key = self.served.to_string();
            Ok(Some(page(SyncMode::Snapshot, &[&key], true)))
        }
    }

    /// C8-05: an answer whose every page comes in its page's time, but
    /// that never ends, is cut off at the sync's budget, not read for a
    /// day: the cache and its cursor stay as the last whole sync left them.
    #[tokio::test]
    async fn an_answer_past_the_sync_s_budget_is_cut_off_and_the_cache_kept() {
        let (_store, federation, peer) = replica("fed-replica-budget").await;
        let whole = vec![page(SyncMode::Snapshot, &["1", "2"], false)];
        sync(
            (&federation, &peer),
            whole,
            false,
            crate::federation::Limits::default().sync,
        )
        .await
        .unwrap();
        let mut slow = Slow {
            every: Duration::from_millis(20),
            served: 0,
        };
        let budget = tokio::time::Instant::now() + Duration::from_millis(300);
        let cut = federation
            .cache_sync(
                &record_of(&federation, &peer).await,
                "plex:1",
                &mut slow,
                budget,
            )
            .await;
        assert!(matches!(cut, Err(ReplicaError::TooSlow(_))), "{cut:?}");
        assert!(slow.served > 1, "pages came in their time");
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

    /// A paired server's answer for ever: pages of `per_page` films,
    /// each film's summary as long as the protocol lets it be, all NULs,
    /// which JSON escapes to six bytes each (C8-02).
    struct Padded {
        per_page: usize,
        served: usize,
    }

    impl PageSource for Padded {
        async fn next_page(&mut self) -> Result<Option<SyncPage>, DialError> {
            let first = self.served * self.per_page;
            self.served += 1;
            let keys: Vec<String> = (first..first + self.per_page)
                .map(|n| n.to_string())
                .collect();
            let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
            let mut page = page(SyncMode::Snapshot, &keys, true);
            for item in &mut page.items {
                item.summary = Some("\0".repeat(spoolframe_protocol::federation::MAX_ITEM_SUMMARY));
            }
            Ok(Some(page.within_bounds()))
        }
    }

    /// C8-02: an answer within the page and item limits, its summaries
    /// padded to the protocol's bound with characters JSON escapes, is
    /// cut off at the page that takes it past its bytes of metadata, not
    /// read to the end and loaded whole: the cache and its cursor stay as
    /// the last whole sync left them.
    #[tokio::test]
    async fn an_answer_past_its_bytes_of_metadata_is_cut_off_at_that_page() {
        let (_store, federation, peer) = replica("fed-replica-bytes").await;
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
        // Each page is 10 films of about 48 KiB of JSON each: the fourth
        // goes past 1.5 MiB.
        federation.set_limits(crate::federation::Limits {
            sync: SyncLimits {
                bytes: 3 << 19,
                ..crate::federation::Limits::default().sync
            },
            ..crate::federation::Limits::default()
        });
        let mut padded = Padded {
            per_page: 10,
            served: 0,
        };
        let cut = federation
            .cache_sync(
                &record_of(&federation, &peer).await,
                "plex:1",
                &mut padded,
                in_budget(),
            )
            .await;
        assert!(
            matches!(
                cut,
                Err(ReplicaError::TooLarge {
                    what: "bytes of metadata",
                    ..
                })
            ),
            "{cut:?}"
        );
        assert_eq!(padded.served, 4, "no page read past it");
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

    /// C8-02: a whole answer that would take the origin's cache past its
    /// bytes of metadata, every library together, is not written.
    #[tokio::test]
    async fn an_answer_past_the_origin_s_bytes_of_metadata_is_not_kept() {
        let (_store, federation, peer) = replica("fed-replica-origin-bytes").await;
        let pair = (&federation, peer.as_str());
        let defaults = crate::federation::Limits::default().sync;
        sync(
            pair,
            vec![page(SyncMode::Snapshot, &["1", "2"], false)],
            false,
            defaults,
        )
        .await
        .unwrap();
        let mut films = page(SyncMode::Changes, &["3", "4"], false);
        for item in &mut films.items {
            item.summary = Some("x".repeat(4_000));
        }
        let small = SyncLimits {
            origin_bytes: 6_000,
            ..defaults
        };
        let cut = sync(pair, vec![films.clone()], false, small).await;
        assert!(
            matches!(
                cut,
                Err(ReplicaError::TooLarge {
                    what: "bytes of metadata cached of the server",
                    ..
                })
            ),
            "{cut:?}"
        );
        assert_eq!(cached(&federation).await, 2);
        sync(pair, vec![films], false, defaults).await.unwrap();
        assert_eq!(cached(&federation).await, 4);
    }

    /// C8-02: the staged items are written a batch at a time, and a sync
    /// of many batches is written as one of a single batch would be: the
    /// new and changed items, those unchanged kept, those gone gone.
    #[tokio::test]
    async fn a_sync_of_many_batches_is_written_whole() {
        let (store, federation, peer) = replica("fed-replica-batches").await;
        let pair = (&federation, peer.as_str());
        let defaults = crate::federation::Limits::default().sync;
        let keys: Vec<String> = (0..350).map(|n| format!("{n:03}")).collect();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        sync(
            pair,
            vec![
                page(SyncMode::Snapshot, &keys[..200], true),
                page(SyncMode::Snapshot, &keys[200..], false),
            ],
            false,
            defaults,
        )
        .await
        .unwrap();
        let (_, total) = store
            .cached_page(ORIGIN, "plex:1", "movie", 0, 1_000)
            .await
            .unwrap();
        assert_eq!(total, 350);
        // Every other film retitled, the last fifty gone.
        let mut changed = page(SyncMode::Changes, &[], false);
        let mut retitled = page(SyncMode::Changes, &keys[..300], false);
        for (n, item) in retitled.items.iter_mut().enumerate() {
            if n % 2 == 0 {
                item.title = format!("Retitled {}", keys[n]);
                *item = item.clone().hashed();
            }
        }
        changed.items = retitled.items;
        changed.removed = keys[300..]
            .iter()
            .map(|key| format!("plex:movie:{key}"))
            .collect();
        sync(pair, vec![changed], false, defaults).await.unwrap();
        let (items, total) = store
            .cached_page(ORIGIN, "plex:1", "movie", 0, 1_000)
            .await
            .unwrap();
        assert_eq!(total, 300);
        let retitled = items
            .iter()
            .filter(|i| i.title.starts_with("Retitled"))
            .count();
        assert_eq!(retitled, 150);
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
