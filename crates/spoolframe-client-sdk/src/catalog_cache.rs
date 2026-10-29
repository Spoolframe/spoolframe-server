//! Catalog reads kept in memory and answered at once, read again behind
//! what is shown once they are old (stale-while-revalidate): Home, the
//! libraries, a library's pages, an item's details and the playlists
//! ([`crate::ClientOptions::catalog_cache`], off by default).
//!
//! A kept value comes back at once. When it is older than its kind's
//! freshness, one re-read starts behind it; when what comes back
//! differs, [`crate::Session::catalog_changes`] says which read changed,
//! and the screen showing it reads it again, getting the new value.
//!
//! Entries are kept per session (server and user), so a profile never
//! sees another's. A change made through the session clears what it
//! touches before it is sent and after it is answered, whatever the
//! answer (a change that timed out may still have been made): watch
//! state clears all but the libraries and the playlists list; a playlist
//! or a heart Home, the items and the playlists; a Home row Home; a
//! pairing everything. The server's playlist, heart and library events
//! clear what they name for every session on that server, and
//! [`crate::Client::watch_changed`] the items it is given, their shows
//! and Home. A read begun before a clear is not kept. Nothing is kept on
//! disk.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use spoolframe_protocol::playlists::PlaylistPage;
use spoolframe_protocol::{HomeScreen, Request};
use spoolframe_types::{Item, Library, LibraryId, MediaId};
use tokio::sync::broadcast;
use tokio::time::Instant;

use crate::{ItemDetail, ItemsPage, PlaylistList, SdkError};

/// The catalog cache's settings ([`crate::ClientOptions::catalog_cache`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheConfig {
    /// The most reads kept, over every session; the least recently read
    /// go first.
    pub capacity: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { capacity: 3000 }
    }
}

/// One read the cache keeps.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CatalogKey {
    Home,
    Libraries,
    /// A page of a library. `order`: its sort and filter, as the SDK
    /// keys them; empty for the plain listing.
    Items {
        library: LibraryId,
        offset: u32,
        limit: u32,
        order: String,
    },
    Item(MediaId),
    Playlists,
    /// A page of a playlist's entries.
    Playlist {
        id: String,
        offset: u32,
        limit: u32,
    },
}

/// A kept read of this session whose re-read came back different: read
/// it again ([`crate::Session::catalog_changes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogChanged {
    pub key: CatalogKey,
}

/// A session's [`CatalogChanged`]s, as they come.
pub struct CatalogChanges {
    scope: Scope,
    changes: Option<broadcast::Receiver<(Scope, CatalogKey)>>,
}

impl CatalogChanges {
    pub(crate) fn of(scope: Scope, catalog: Option<&CatalogCache>) -> Self {
        Self {
            scope,
            changes: catalog.map(CatalogCache::changes),
        }
    }

    /// The next change; `None` once the client is gone, and at once with
    /// the cache off. Changes missed while not listening are skipped: the
    /// screen reads again what it shows.
    pub async fn recv(&mut self) -> Option<CatalogChanged> {
        let changes = self.changes.as_mut()?;
        loop {
            match changes.recv().await {
                Ok((scope, key)) if scope == self.scope => return Some(CatalogChanged { key }),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

/// A read's answer, as kept.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Home(Box<HomeScreen>),
    Libraries(Vec<Library>),
    Items(ItemsPage),
    Item(Box<ItemDetail>),
    Playlists(Box<PlaylistList>),
    Playlist(Box<PlaylistPage>),
}

impl Value {
    /// How long it is shown before a re-read starts behind it: what
    /// changes often (Home, the playlists) soonest, music latest.
    fn freshness(&self) -> Duration {
        const SECOND: Duration = Duration::from_secs(1);
        const MINUTE: Duration = Duration::from_secs(60);
        match self {
            Value::Home(_) => 10 * SECOND,
            Value::Playlists(_) | Value::Playlist(_) => 30 * SECOND,
            Value::Items(_) => 2 * MINUTE,
            Value::Libraries(_) => 10 * MINUTE,
            Value::Item(detail) => match detail.item {
                Item::Movie(_) | Item::Episode(_) | Item::Show(_) | Item::Season(_) => 5 * MINUTE,
                Item::Artist(_) | Item::Album(_) | Item::Track(_) => 10 * MINUTE,
            },
        }
    }
}

/// What a change clears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Clear {
    /// Watch state: all but the libraries and the playlists list.
    Watch,
    /// A heart, or a playlist's entries: Home, the items and the
    /// playlists.
    Items,
    /// Playlist `id` changed or went: its pages, the list, the items
    /// (which name their playlists) and Home.
    Playlist(String),
    /// Pinned changed: the list and Home.
    Pins,
    /// A Home row.
    Home,
    /// Items whose watch state changed elsewhere, their shows and
    /// seasons with them: those and Home.
    Watched(HashSet<MediaId>),
    Everything,
}

impl Clear {
    fn clears(&self, key: &CatalogKey) -> bool {
        use CatalogKey as K;
        match self {
            Clear::Everything => true,
            Clear::Home => matches!(key, K::Home),
            Clear::Pins => matches!(key, K::Home | K::Playlists),
            Clear::Items => matches!(
                key,
                K::Home | K::Item(_) | K::Playlists | K::Playlist { .. }
            ),
            Clear::Playlist(id) => match key {
                K::Home | K::Item(_) | K::Playlists => true,
                K::Playlist { id: kept, .. } => kept == id,
                _ => false,
            },
            Clear::Watch => !matches!(key, K::Libraries | K::Playlists),
            Clear::Watched(ids) => match key {
                K::Home => true,
                K::Item(id) => ids.contains(id),
                _ => false,
            },
        }
    }
}

/// What `request` changes of what is kept, if anything.
pub(crate) fn clears(request: &Request) -> Option<Clear> {
    use Request as R;
    Some(match request {
        // A paired server's item, reported to this one: that item, not the
        // whole of watch state.
        R::UpdateWatchProgress { id, .. } => Clear::Watched(HashSet::from([id.clone()])),
        R::StartPlayback { .. }
        | R::StopPlayback { .. }
        | R::MarkWatched { .. }
        | R::MarkUnwatched { .. }
        | R::HideFromContinueWatching { .. }
        | R::UnhideFromContinueWatching { .. }
        | R::SetPlayFrom { .. }
        | R::PlayPlaylist { .. }
        | R::NextInPlaylist { .. }
        | R::SyncOfflineWatch { .. } => Clear::Watch,
        R::CreatePlaylist { .. }
        | R::UpdatePlaylist { .. }
        | R::EditPlaylist { .. }
        | R::DeletePlaylist { .. }
        | R::SetPlaylistSettings { .. }
        | R::ImportPlexPlaylists { .. }
        | R::RemovePlaylistArt { .. }
        | R::Like { .. }
        | R::Unlike { .. }
        | R::SyncOfflinePlays { .. } => Clear::Items,
        R::PinPlaylist { .. } | R::UnpinPlaylist { .. } | R::DismissPinNotes => Clear::Pins,
        R::SetServerRow { .. } => Clear::Home,
        R::PairServer(_) | R::ShareLibraries(_) | R::LeaveServer | R::Logout => Clear::Everything,
        _ => return None,
    })
}

/// Whose entries: one user signed in on one server.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Scope {
    pub(crate) server: String,
    pub(crate) user: String,
}

type Key = (Scope, CatalogKey);

struct Entry {
    /// Shared, so a read clones the `Arc` under the lock, not the value.
    value: Arc<Value>,
    fetched: Instant,
    last_read: u64,
    /// A re-read of it is in flight.
    revalidating: bool,
}

#[derive(Default)]
struct State {
    entries: HashMap<Key, Entry>,
    /// Each entry by its `last_read`, oldest first: what is evicted.
    by_read: BTreeMap<u64, Key>,
    tick: u64,
    /// One more for each clear: a read begun before it is not kept.
    generations: HashMap<Scope, u64>,
}

impl State {
    fn generation(&self, scope: &Scope) -> u64 {
        self.generations.get(scope).copied().unwrap_or(0)
    }

    fn scopes_of(&self, server: &str) -> Vec<Scope> {
        self.entries
            .keys()
            .map(|(s, _)| s)
            .chain(self.generations.keys())
            .filter(|s| s.server == server)
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    fn clear(&mut self, scope: &Scope, clear: &Clear) {
        let by_read = &mut self.by_read;
        self.entries.retain(|(s, key), entry| {
            let keep = s != scope || !clear.clears(key);
            if !keep {
                by_read.remove(&entry.last_read);
            }
            keep
        });
        *self.generations.entry(scope.clone()).or_default() += 1;
    }

    /// `key` was read now.
    fn touch(&mut self, key: &Key) {
        self.tick += 1;
        let tick = self.tick;
        if let Some(entry) = self.entries.get_mut(key) {
            self.by_read.remove(&entry.last_read);
            entry.last_read = tick;
            self.by_read.insert(tick, key.clone());
        }
    }

    /// Keep `entry` as `key`, read now; past `capacity`, the entry read
    /// longest ago goes.
    fn insert(&mut self, key: Key, mut entry: Entry, capacity: usize) {
        self.tick += 1;
        entry.last_read = self.tick;
        self.by_read.insert(self.tick, key.clone());
        if let Some(old) = self.entries.insert(key, entry) {
            self.by_read.remove(&old.last_read);
        }
        if self.entries.len() > capacity
            && let Some((_, oldest)) = self.by_read.pop_first()
        {
            self.entries.remove(&oldest);
        }
    }
}

/// The kept reads of every session.
pub(crate) struct CatalogCache {
    state: Mutex<State>,
    changes: broadcast::Sender<(Scope, CatalogKey)>,
    runtime: tokio::runtime::Handle,
    capacity: usize,
}

impl CatalogCache {
    pub(crate) fn new(runtime: tokio::runtime::Handle, config: CacheConfig) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            changes: broadcast::channel(64).0,
            runtime,
            capacity: config.capacity,
        })
    }

    fn changes(&self) -> broadcast::Receiver<(Scope, CatalogKey)> {
        self.changes.subscribe()
    }

    /// Forget what `clear` covers of `scope`'s reads; reads in flight are
    /// not kept.
    pub(crate) fn clear(&self, scope: &Scope, clear: &Clear) {
        self.state.lock().clear(scope, clear);
    }

    /// [`Self::clear`] for every user of `server`.
    pub(crate) fn clear_server(&self, server: &str, clear: &Clear) {
        let mut state = self.state.lock();
        for scope in state.scopes_of(server) {
            state.clear(&scope, clear);
        }
    }

    /// The watch state of `items` changed on `server`: those, the shows
    /// and seasons of kept episodes and seasons among them, and Home go.
    pub(crate) fn watch_changed(&self, server: &str, items: &[MediaId]) {
        let mut state = self.state.lock();
        for scope in state.scopes_of(server) {
            let mut ids: HashSet<MediaId> = items.iter().cloned().collect();
            for id in items {
                let key = (scope.clone(), CatalogKey::Item(id.clone()));
                if let Some(Value::Item(detail)) = state.entries.get(&key).map(|e| &*e.value) {
                    match &detail.item {
                        Item::Episode(e) => {
                            ids.insert(e.show_id.clone());
                            ids.insert(e.season_id.clone());
                        }
                        Item::Season(s) => {
                            ids.insert(s.show_id.clone());
                        }
                        _ => {}
                    }
                }
            }
            state.clear(&scope, &Clear::Watched(ids));
        }
    }

    /// Forget everything.
    pub(crate) fn clear_all(&self) {
        let mut state = self.state.lock();
        state.entries.clear();
        state.by_read.clear();
        for generation in state.generations.values_mut() {
            *generation += 1;
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.state.lock().entries.len()
    }

    /// The kept value of `read` at once, a re-read started behind it when
    /// it is old; else `fetch`ed now, and kept.
    pub(crate) async fn read<F, Fut>(
        self: &Arc<Self>,
        scope: &Scope,
        read: CatalogKey,
        fetch: F,
    ) -> Result<Value, SdkError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value, SdkError>> + Send + 'static,
    {
        let key = (scope.clone(), read);
        let now = Instant::now();
        let (kept, generation, revalidate) = {
            let mut state = self.state.lock();
            state.touch(&key);
            let generation = state.generation(scope);
            match state.entries.get_mut(&key) {
                Some(entry) => {
                    let stale = now.duration_since(entry.fetched) >= entry.value.freshness();
                    let revalidate = stale && !entry.revalidating;
                    entry.revalidating |= revalidate;
                    (Some(entry.value.clone()), generation, revalidate)
                }
                None => (None, generation, false),
            }
        };
        if let Some(value) = kept {
            let value = Value::clone(&value);
            if revalidate {
                let (cache, fetching) = (self.clone(), fetch());
                self.runtime.spawn(async move {
                    let fetched = fetching.await;
                    cache.revalidated(key, generation, fetched);
                });
            }
            return Ok(value);
        }
        let value = fetch().await?;
        self.keep(key, generation, value.clone());
        Ok(value)
    }

    /// Keep a value read now, unless its scope was cleared since the read
    /// began.
    fn keep(&self, key: Key, generation: u64, value: Value) {
        let mut state = self.state.lock();
        if state.generation(&key.0) != generation {
            return;
        }
        let entry = Entry {
            value: Arc::new(value),
            fetched: Instant::now(),
            last_read: 0,
            revalidating: false,
        };
        state.insert(key, entry, self.capacity);
    }

    /// A re-read is in: kept, and said when it differs.
    fn revalidated(&self, key: Key, generation: u64, fetched: Result<Value, SdkError>) {
        let changed = {
            let mut state = self.state.lock();
            if state.generation(&key.0) != generation {
                return;
            }
            let Some(entry) = state.entries.get_mut(&key) else {
                return;
            };
            entry.revalidating = false;
            match fetched {
                Ok(value) => {
                    let changed = *entry.value != value;
                    entry.value = Arc::new(value);
                    entry.fetched = Instant::now();
                    changed
                }
                Err(error) => {
                    // The kept value stays; the next read tries again.
                    tracing::debug!(%error, read = ?key.1, "a catalog re-read failed");
                    false
                }
            }
        };
        if changed {
            let (scope, read) = key;
            // Nobody listening is fine: nothing shows it.
            let _ = self.changes.send((scope, read));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spoolframe_types::{LibraryKind, Namespace};
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    fn scope(user: &str) -> Scope {
        Scope {
            server: "srv".into(),
            user: user.into(),
        }
    }

    fn libraries(title: &str) -> Value {
        Value::Libraries(vec![Library {
            id: LibraryId::new(Namespace::Plex, "1").unwrap(),
            title: title.into(),
            kind: LibraryKind::Movies,
            server: None,
        }])
    }

    fn home() -> Value {
        Value::Home(Box::new(HomeScreen {
            continue_watching: Vec::new(),
            next_up: Vec::new(),
            recently_added: Vec::new(),
            unavailable: Vec::new(),
            servers: Vec::new(),
        }))
    }

    /// A server whose answer the test sets, counting the reads.
    #[derive(Default)]
    struct Server {
        reads: AtomicUsize,
        title: Mutex<String>,
    }

    impl Server {
        fn fetch(
            self: &Arc<Self>,
        ) -> impl Future<Output = Result<Value, SdkError>> + Send + 'static {
            let server = self.clone();
            async move {
                server.reads.fetch_add(1, SeqCst);
                Ok(libraries(&server.title.lock()))
            }
        }
    }

    async fn read(cache: &Arc<CatalogCache>, server: &Arc<Server>, who: &str) -> Value {
        cache
            .read(&scope(who), CatalogKey::Libraries, || server.fetch())
            .await
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn a_fresh_read_is_answered_from_what_was_kept() {
        let cache = CatalogCache::new(tokio::runtime::Handle::current(), CacheConfig::default());
        let server = Arc::new(Server::default());
        read(&cache, &server, "alice").await;
        read(&cache, &server, "alice").await;
        assert_eq!(server.reads.load(SeqCst), 1);
        // Another profile's is its own.
        read(&cache, &server, "bob").await;
        assert_eq!(server.reads.load(SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_read_is_shown_and_read_again_once_behind_it() {
        let cache = CatalogCache::new(tokio::runtime::Handle::current(), CacheConfig::default());
        let mut changes = cache.changes();
        let server = Arc::new(Server::default());
        *server.title.lock() = "Films".into();
        read(&cache, &server, "alice").await;
        tokio::time::advance(Duration::from_secs(600)).await;
        *server.title.lock() = "Movies".into();
        assert_eq!(read(&cache, &server, "alice").await, libraries("Films"));
        assert_eq!(read(&cache, &server, "alice").await, libraries("Films"));
        let change = changes.recv().await.unwrap();
        assert_eq!(change.1, CatalogKey::Libraries);
        assert_eq!(
            server.reads.load(SeqCst),
            2,
            "one re-read for two stale reads"
        );
        assert_eq!(read(&cache, &server, "alice").await, libraries("Movies"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_re_read_that_finds_nothing_new_says_nothing() {
        let cache = CatalogCache::new(tokio::runtime::Handle::current(), CacheConfig::default());
        let mut changes = cache.changes();
        let server = Arc::new(Server::default());
        read(&cache, &server, "alice").await;
        tokio::time::advance(Duration::from_secs(600)).await;
        read(&cache, &server, "alice").await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(server.reads.load(SeqCst), 2);
        assert!(changes.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_watch_change_clears_home_but_keeps_the_libraries() {
        let cache = CatalogCache::new(tokio::runtime::Handle::current(), CacheConfig::default());
        let server = Arc::new(Server::default());
        read(&cache, &server, "alice").await;
        cache
            .read(&scope("alice"), CatalogKey::Home, || async { Ok(home()) })
            .await
            .unwrap();
        cache.clear(&scope("alice"), &Clear::Watch);
        assert_eq!(cache.len(), 1);
        cache.clear_server("srv", &Clear::Everything);
        assert_eq!(cache.len(), 0);
    }

    /// A read begun before a clear answers after it: it is not kept, so
    /// the next read is the server's.
    #[tokio::test(start_paused = true)]
    async fn a_read_begun_before_a_clear_is_not_kept() {
        let cache = CatalogCache::new(tokio::runtime::Handle::current(), CacheConfig::default());
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let reading = {
            let cache = cache.clone();
            tokio::spawn(async move {
                cache
                    .read(&scope("alice"), CatalogKey::Libraries, || async move {
                        let _ = held.await;
                        Ok(libraries("old"))
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        cache.clear(&scope("alice"), &Clear::Everything);
        release.send(()).unwrap();
        reading.await.unwrap().unwrap();
        assert_eq!(cache.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_least_recently_read_goes_first_past_capacity() {
        let cache = CatalogCache::new(
            tokio::runtime::Handle::current(),
            CacheConfig { capacity: 2 },
        );
        let server = Arc::new(Server::default());
        read(&cache, &server, "a").await;
        read(&cache, &server, "b").await;
        read(&cache, &server, "a").await;
        read(&cache, &server, "c").await;
        assert_eq!(cache.len(), 2);
        // b went; a is still kept.
        read(&cache, &server, "a").await;
        assert_eq!(server.reads.load(SeqCst), 3);
    }

    fn detail(item: Item) -> Value {
        Value::Item(Box::new(ItemDetail {
            item,
            children: Vec::new(),
            markers: Vec::new(),
            playlists: Vec::new(),
            liked: None,
            liked_children: Vec::new(),
            copies: None,
        }))
    }

    fn id(kind: spoolframe_types::MediaKind, key: &str) -> MediaId {
        MediaId::new(Namespace::Plex, kind, key).unwrap()
    }

    fn episode() -> Item {
        use spoolframe_types::{Episode, MediaKind, PlaybackState};
        Item::Episode(Episode {
            id: id(MediaKind::Episode, "e1"),
            show_id: id(MediaKind::Show, "s"),
            show_title: None,
            season_id: id(MediaKind::Season, "s1"),
            season: 1,
            episode: 1,
            title: "Pilot".into(),
            summary: None,
            content_rating: None,
            year: None,
            air_date: None,
            duration: None,
            cast: Vec::new(),
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        })
    }

    async fn keep(cache: &Arc<CatalogCache>, who: &str, key: CatalogKey, value: Value) {
        cache
            .read(&scope(who), key, move || async move { Ok(value) })
            .await
            .unwrap();
    }

    fn kept(cache: &CatalogCache, who: &str, key: &CatalogKey) -> bool {
        cache
            .state
            .lock()
            .entries
            .contains_key(&(scope(who), key.clone()))
    }

    #[tokio::test(start_paused = true)]
    async fn a_watch_change_elsewhere_clears_the_items_their_show_and_home() {
        use spoolframe_types::MediaKind;
        let cache = CatalogCache::new(tokio::runtime::Handle::current(), CacheConfig::default());
        let episode_key = CatalogKey::Item(id(MediaKind::Episode, "e1"));
        let show = CatalogKey::Item(id(MediaKind::Show, "s"));
        let other = CatalogKey::Item(id(MediaKind::Show, "other"));
        for who in ["alice", "bob"] {
            keep(&cache, who, episode_key.clone(), detail(episode())).await;
            keep(&cache, who, show.clone(), libraries("not looked at")).await;
            keep(&cache, who, other.clone(), libraries("not looked at")).await;
            keep(&cache, who, CatalogKey::Home, home()).await;
            keep(&cache, who, CatalogKey::Libraries, libraries("Films")).await;
        }
        cache.watch_changed("srv", &[id(MediaKind::Episode, "e1")]);
        for who in ["alice", "bob"] {
            assert!(!kept(&cache, who, &episode_key));
            assert!(!kept(&cache, who, &show), "its show goes too");
            assert!(!kept(&cache, who, &CatalogKey::Home));
            assert!(kept(&cache, who, &other));
            assert!(kept(&cache, who, &CatalogKey::Libraries));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_playlist_event_clears_that_playlist_and_the_list_only() {
        let cache = CatalogCache::new(tokio::runtime::Handle::current(), CacheConfig::default());
        let page = |id: &str| CatalogKey::Playlist {
            id: id.into(),
            offset: 0,
            limit: 50,
        };
        keep(&cache, "alice", page("a"), libraries("a")).await;
        keep(&cache, "alice", page("b"), libraries("b")).await;
        keep(&cache, "alice", CatalogKey::Playlists, libraries("list")).await;
        cache.clear_server("srv", &Clear::Playlist("a".into()));
        assert!(!kept(&cache, "alice", &page("a")));
        assert!(!kept(&cache, "alice", &CatalogKey::Playlists));
        assert!(kept(&cache, "alice", &page("b")));
    }

    #[tokio::test(start_paused = true)]
    async fn an_episode_stays_fresh_five_minutes_playlists_half_a_minute() {
        assert_eq!(detail(episode()).freshness(), Duration::from_secs(300));
        assert_eq!(
            Value::Playlists(Box::new(PlaylistList {
                playlists: Vec::new(),
                pin_notes: Vec::new(),
            }))
            .freshness(),
            Duration::from_secs(30)
        );
        assert_eq!(home().freshness(), Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn a_sessions_changes_are_its_own() {
        let cache = CatalogCache::new(tokio::runtime::Handle::current(), CacheConfig::default());
        let mut alices = CatalogChanges::of(scope("alice"), Some(&cache));
        let _ = cache.changes.send((scope("bob"), CatalogKey::Home));
        let _ = cache.changes.send((scope("alice"), CatalogKey::Libraries));
        assert_eq!(
            alices.recv().await,
            Some(CatalogChanged {
                key: CatalogKey::Libraries
            })
        );
        let mut off = CatalogChanges::of(scope("alice"), None);
        assert_eq!(off.recv().await, None);
    }

    #[test]
    fn each_change_clears_what_it_touches() {
        let id: MediaId = "plex:movie:1".parse().unwrap();
        assert_eq!(
            clears(&Request::MarkWatched { id: id.clone() }),
            Some(Clear::Watch)
        );
        // A paired server's item reported here: only it, and Home.
        assert_eq!(
            clears(&Request::UpdateWatchProgress {
                id: id.clone(),
                position_ms: 0
            }),
            Some(Clear::Watched(HashSet::from([id.clone()])))
        );
        assert_eq!(clears(&Request::Item { id }), None);
        assert_eq!(clears(&Request::DismissPinNotes), Some(Clear::Pins));
        assert!(Clear::Items.clears(&CatalogKey::Home));
        assert!(!Clear::Items.clears(&CatalogKey::Libraries));
        assert!(!Clear::Home.clears(&CatalogKey::Libraries));
    }
}
