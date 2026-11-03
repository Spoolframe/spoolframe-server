//! All (PX-43) on a large music library: two libraries of 10,000 tracks
//! each, listed at once as the Plex catalog lists them, with files that
//! take a while to look at.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use spoolframe_catalog::{
    Catalog, CatalogError, Found, Image, ItemFile, ItemPage, ItemWithChildren, Page, SearchQuery,
};
use spoolframe_media::Prober;
use spoolframe_media::sandbox::Tool;
use spoolframe_protocol::playlists::Builtin;
use spoolframe_types::{
    ArtworkId, Item, Library, LibraryId, LibraryKind, MediaId, MediaKind, Namespace, PersonId,
    PlaybackState, Role, Track,
};

use super::*;
use crate::principal::{Actor, Principal};
use crate::test_support::fake_ffprobe;
use crate::watch::{Watch, WatchRules};

/// Tracks in each of the two libraries.
const TRACKS: u32 = 10_000;

/// The key of the one track whose file cannot be read.
const UNREADABLE: u32 = 1_000_003;

/// Library `n`'s track `t` is `plex:track:<n*1000000+t>`.
fn track_key(library: u32, track: u32) -> u32 {
    library * 1_000_000 + track
}

/// Music libraries `plex:library:1` and `:2`, each of [`TRACKS`] tracks
/// by 50 artists, in no order, listed whole by `tracks`.
#[derive(Debug)]
struct Big {
    /// How many tracks each library has now.
    sizes: Mutex<[u32; 2]>,
    /// How long listing a library's tracks takes.
    list_takes: Duration,
    /// How long looking at a file takes.
    file_takes: Duration,
    /// Whether `items_with_files` answers, as the Plex catalog's does.
    answers_many: AtomicBool,
    /// `items_with_files` calls so far.
    asked_many: AtomicUsize,
    /// The most items `items_with_files` was asked of.
    most_many: AtomicUsize,
    /// `item` calls so far.
    items_alone: AtomicUsize,
    /// `media_path` calls so far.
    files_alone: AtomicUsize,
    /// `tracks` calls so far.
    listed: AtomicUsize,
    /// Held by a test to keep `tracks` from finishing.
    listing: tokio::sync::RwLock<()>,
    /// Whether `tracks` fails now.
    down: AtomicBool,
    /// Files being looked at now, and the most at once.
    looking: AtomicUsize,
    most_looking: AtomicUsize,
}

impl Big {
    fn new(list_takes: Duration, file_takes: Duration) -> Self {
        Self {
            sizes: Mutex::new([TRACKS; 2]),
            list_takes,
            file_takes,
            answers_many: AtomicBool::new(false),
            asked_many: AtomicUsize::new(0),
            most_many: AtomicUsize::new(0),
            items_alone: AtomicUsize::new(0),
            files_alone: AtomicUsize::new(0),
            listed: AtomicUsize::new(0),
            listing: tokio::sync::RwLock::new(()),
            down: AtomicBool::new(false),
            looking: AtomicUsize::new(0),
            most_looking: AtomicUsize::new(0),
        }
    }

    /// Answers for many items at once, as Plex's catalog does.
    fn answering_many(self) -> Self {
        self.answers_many.store(true, Ordering::SeqCst);
        self
    }

    /// A scan found one more track in library `n`.
    fn add_track(&self, library: u32) {
        self.sizes.lock()[library as usize - 1] += 1;
    }

    fn library(n: u32) -> Library {
        Library {
            id: LibraryId::new(Namespace::Plex, n.to_string()).unwrap(),
            title: format!("Music {n}"),
            kind: LibraryKind::Music,
            server: None,
        }
    }

    fn track(library: u32, t: u32) -> Item {
        let artist = (t * 7919) % 50;
        let album = artist * 1000 + (t / 12) % 20;
        Item::Track(Track {
            id: MediaId::new(
                Namespace::Plex,
                MediaKind::Track,
                track_key(library, t).to_string(),
            )
            .unwrap(),
            album_id: MediaId::new(Namespace::Plex, MediaKind::Album, album.to_string()).unwrap(),
            artist_id: MediaId::new(Namespace::Plex, MediaKind::Artist, artist.to_string())
                .unwrap(),
            artist_name: Some(format!("Artist {artist}")),
            album_title: Some(format!("Album {album}")),
            title: format!("Track {t}"),
            track: Some(t % 12 + 1),
            disc: Some(1),
            album_track_count: None,
            codec: None,
            bit_depth: None,
            sample_rate: None,
            duration: None,
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        })
    }

    /// The track under `id` now, if there is one.
    fn find(&self, id: &MediaId) -> Option<Item> {
        let key: u32 = id.key().parse().ok()?;
        let (library, t) = (key / 1_000_000, key % 1_000_000);
        let sizes = *self.sizes.lock();
        (id.kind() == MediaKind::Track
            && (1..=2).contains(&library)
            && (1..=sizes[library as usize - 1]).contains(&t))
        .then(|| Self::track(library, t))
    }
}

#[async_trait]
impl Catalog for Big {
    async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
        Ok(vec![Self::library(1), Self::library(2)])
    }

    async fn items(&self, _: &LibraryId, _: Page) -> Result<ItemPage, CatalogError> {
        Ok(ItemPage {
            items: Vec::new(),
            total: 0,
        })
    }

    async fn tracks(&self, library: &LibraryId) -> Result<Vec<Item>, CatalogError> {
        self.listed.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.list_takes).await;
        let _listing = self.listing.read().await;
        if self.down.load(Ordering::SeqCst) {
            return Err(CatalogError::NotSetUp);
        }
        let n: u32 = library.key().parse().unwrap();
        let size = self.sizes.lock()[n as usize - 1];
        Ok((1..=size).map(|t| Self::track(n, t)).collect())
    }

    async fn item(&self, id: &MediaId) -> Result<ItemWithChildren, CatalogError> {
        self.items_alone.fetch_add(1, Ordering::SeqCst);
        let item = self
            .find(id)
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))?;
        Ok(ItemWithChildren {
            external_ids: Vec::new(),
            item,
            children: Vec::new(),
            markers: Vec::new(),
        })
    }

    async fn external_ids(&self, _: &MediaId) -> Result<Vec<String>, CatalogError> {
        Ok(Vec::new())
    }

    async fn search(&self, _: &SearchQuery) -> Result<Found, CatalogError> {
        Ok(Found::default())
    }

    async fn person_items(&self, id: &PersonId) -> Result<Vec<Item>, CatalogError> {
        Err(CatalogError::NotFound(id.to_string()))
    }

    /// Every file is there, after [`Big::file_takes`].
    async fn media_path(&self, id: &MediaId) -> Result<PathBuf, CatalogError> {
        self.files_alone.fetch_add(1, Ordering::SeqCst);
        let now = self.looking.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_looking.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(self.file_takes).await;
        self.looking.fetch_sub(1, Ordering::SeqCst);
        Ok(PathBuf::from(format!("/music/{}.flac", id.key())))
    }

    async fn items_with_files(&self, ids: &[MediaId]) -> HashMap<MediaId, ItemFile> {
        self.asked_many.fetch_add(1, Ordering::SeqCst);
        self.most_many.fetch_max(ids.len(), Ordering::SeqCst);
        if !self.answers_many.load(Ordering::SeqCst) {
            return HashMap::new();
        }
        ids.iter()
            .filter_map(|id| {
                let readable = id.key() != UNREADABLE.to_string();
                let mut item = self.find(id)?;
                // What Plex says of a track alone and not in a listing.
                if let Item::Track(track) = &mut item {
                    track.sample_rate = Some(96_000);
                }
                Some((id.clone(), ItemFile { item, readable }))
            })
            .collect()
    }

    async fn recently_added(&self, _: u32) -> Result<Vec<Item>, CatalogError> {
        Ok(Vec::new())
    }

    async fn authorize_artwork(&self, _: &ArtworkId) -> Result<(), CatalogError> {
        Ok(())
    }

    async fn artwork(&self, id: &ArtworkId) -> Result<Image, CatalogError> {
        Err(CatalogError::NotFound(id.to_string()))
    }
}

struct Harness {
    // Declared first, so the store closes before its directory goes.
    playlists: Playlists,
    music: Arc<Big>,
    alice: Actor,
    all: String,
    _dir: spoolframe_test_dir::TestDir,
}

/// Alice and her All, on `music`; `kept` makes the playlists as wanted.
async fn harness(test: &str, music: Big, kept: impl FnOnce(Playlists) -> Playlists) -> Harness {
    let dir = crate::store::tests::state_dir(&format!("all-{test}")).await;
    let store = Store::open(dir.as_ref()).await.unwrap();
    let music = Arc::new(music);
    let ffprobe = fake_ffprobe(dir.as_ref(), "mkv-forced-subs.json");
    let prober = Arc::new(Prober::new(
        Tool::unconfined(ffprobe),
        Prober::DEFAULT_TIMEOUT,
    ));
    let watch = Arc::new(Watch::new(
        store.clone(),
        music.clone(),
        prober,
        WatchRules::default(),
    ));
    let id = store
        .add_user("alice", None, "phc", Role::Consumer)
        .await
        .unwrap();
    let alice = Actor {
        principal: Principal::LocalUser(id),
        admin: false,
        federated: false,
    };
    let playlists = kept(Playlists::new(store, music.clone(), watch));
    let (list, _) = playlists.list(&alice).await.unwrap();
    let all = list
        .into_iter()
        .find(|p| p.builtin == Some(Builtin::All))
        .expect("listed")
        .id;
    Harness {
        playlists,
        music,
        alice,
        all,
        _dir: dir,
    }
}

impl Harness {
    async fn page(&self, offset: u32) -> PlaylistPage {
        self.playlists
            .get(&self.alice, &self.all, offset, MAX_PAGE)
            .await
            .unwrap()
    }

    fn listed(&self) -> usize {
        self.music.listed.load(Ordering::SeqCst)
    }

    /// A stored playlist of library 1's tracks `tracks`.
    async fn stored(&self, tracks: &[u32]) -> String {
        self.stored_with_gone(tracks, 0).await
    }

    /// [`Self::stored`], with an entry for library 1's track `gone`,
    /// which there is none of, when it is not 0.
    async fn stored_with_gone(&self, tracks: &[u32], gone: u32) -> String {
        let store = &self.playlists.store;
        let row = store
            .create_playlist(&NewPlaylist {
                owner: self.alice.principal.to_string(),
                name: "Stored".into(),
                kinds: super::TRACKS,
                server_wide: false,
                shares: Vec::new(),
            })
            .await
            .unwrap();
        let mut entries: Vec<NewEntry> = tracks
            .iter()
            .map(|t| new_entry(&Big::track(1, *t), Vec::new()))
            .collect();
        if gone != 0 {
            entries.push(new_entry(&Big::track(1, gone), Vec::new()));
        }
        store
            .edit_playlist(
                &row.id,
                row.revision,
                vec![EntryEdit::Add {
                    entries,
                    before: None,
                }],
            )
            .await
            .unwrap();
        row.id
    }

    /// Entries of a stored playlist of library 1's first tracks.
    async fn first_tracks_of_library_1(&self) -> Vec<PlaylistEntry> {
        let id = self.stored(&[1, 2, 3, 4]).await;
        let page = self.playlists.get(&self.alice, &id, 0, 100);
        page.await.unwrap().entries
    }

    /// A page, failing the test if it waits on a listing.
    async fn page_at_once(&self, offset: u32) -> PlaylistPage {
        tokio::time::timeout(WAITS_FOR, self.page(offset))
            .await
            .expect("the read waited for the libraries to be listed")
    }

    /// Until All has `total` tracks, listed behind the reads that asked.
    async fn until_total(&self, total: u32) {
        tokio::time::timeout(WAITS_FOR, async {
            while self.page(0).await.total != total {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the list never came");
    }

    /// Until All's list is kept, as `until_listed` can't tell: `tracks` is
    /// counted on the way in, before its list comes back.
    async fn until_kept(&self) {
        tokio::time::timeout(WAITS_FOR, async {
            while self.playlists.all.kept.lock().is_none() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the list was never kept");
    }

    /// Until `tracks` was asked `times` times.
    async fn until_listed(&self, times: usize) {
        tokio::time::timeout(WAITS_FOR, async {
            while self.listed() < times {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the libraries were never listed");
    }
}

/// Library 1's track of `key`.
fn plex_track(key: u32) -> MediaId {
    MediaId::new(Namespace::Plex, MediaKind::Track, key.to_string()).unwrap()
}

/// The time a test gives what should take none. Only a failure waits it.
const WAITS_FOR: Duration = Duration::from_secs(10);

/// Moves Tokio's clock past [`ALL_KEPT_FOR`](super::ALL_KEPT_FOR), not
/// slept through: a slow machine can't age the list between two reads
/// that must share it.
async fn age_the_list() {
    tokio::time::pause();
    tokio::time::advance(super::ALL_KEPT_FOR).await;
    tokio::time::resume();
}

/// The bug: each page listed every track of every library again, which
/// on a large library took longer than a client waits.
#[tokio::test]
async fn a_second_page_of_all_does_not_list_the_libraries_again() {
    let h = harness(
        "second-page",
        Big::new(Duration::ZERO, Duration::ZERO),
        |p| p,
    )
    .await;
    let first = h.page(0).await;
    assert_eq!(first.total, 2 * TRACKS);
    let listed = h.listed();
    let second = h.page(MAX_PAGE).await;
    assert_eq!(second.offset, MAX_PAGE);
    assert_eq!(h.listed(), listed, "the second page listed the libraries");
}

/// Playing All and listing what comes up next use the same list.
#[tokio::test]
async fn playing_all_and_its_upcoming_do_not_list_the_libraries_again() {
    let h = harness("play", Big::new(Duration::ZERO, Duration::ZERO), |p| p).await;
    h.page(0).await;
    let listed = h.listed();
    let first = h
        .playlists
        .play(&h.alice, &h.all, None, PlaySettings::default())
        .await
        .unwrap()
        .unwrap();
    h.playlists
        .next(&h.alice, &h.all, first.entry)
        .await
        .unwrap()
        .unwrap();
    h.playlists.upcoming(&h.alice, &h.all, 10).await.unwrap();
    assert_eq!(h.listed(), listed);
}

/// A listing leaves out a track's stream details: a page has what asking
/// of its files brought, on All and on a long stored playlist.
#[tokio::test]
async fn a_page_keeps_what_asking_of_its_files_brought() {
    let music = Big::new(Duration::ZERO, Duration::ZERO).answering_many();
    let h = harness("details", music, |p| p).await;
    let rate = |entry: &PlaylistEntry| match &entry.item {
        Item::Track(track) => track.sample_rate,
        _ => None,
    };
    let all = h.page(0).await;
    assert_eq!(rate(&all.entries[0]), Some(96_000));
    let keys: Vec<u32> = (1..=300).collect();
    let id = h.stored(&keys).await;
    let page = h.playlists.get(&h.alice, &id, 0, 100).await.unwrap();
    assert_eq!(rate(&page.entries[0]), Some(96_000));
}

/// A page's 200 files are looked at a few at once, as a playlist's own
/// entries are, not one after another.
#[tokio::test]
async fn a_page_of_all_looks_at_its_files_a_few_at_once() {
    let h = harness(
        "files",
        Big::new(Duration::ZERO, Duration::from_millis(5)),
        |p| p,
    )
    .await;
    let page = h.page(0).await;
    assert_eq!(page.entries.len(), MAX_PAGE as usize);
    let most = h.music.most_looking.load(Ordering::SeqCst);
    assert!(
        (2..=CATALOG_FETCHES).contains(&most),
        "{most} files looked at at once"
    );
}

/// A page of All on a catalog that answers for many at once is one
/// question, however many it lists; 100 requests were 90% of a read.
#[tokio::test]
async fn a_page_of_all_asks_of_its_files_once() {
    let music = Big::new(Duration::ZERO, Duration::ZERO).answering_many();
    let h = harness("many", music, |p| p).await;
    h.page(0).await;
    assert_eq!(h.music.asked_many.load(Ordering::SeqCst), 1);
    assert_eq!(h.music.files_alone.load(Ordering::SeqCst), 0);
}

/// One whose file cannot be read is said so.
#[tokio::test]
async fn a_page_says_which_files_cannot_be_read() {
    let music = Big::new(Duration::ZERO, Duration::ZERO).answering_many();
    let h = harness("many-unreadable", music, |p| p).await;
    let unreadable = plex_track(UNREADABLE);
    let page = h.first_tracks_of_library_1().await;
    let entry = page.iter().find(|e| *e.item.id() == unreadable).unwrap();
    assert!(entry.unavailable);
    assert_eq!(
        page.iter().filter(|e| e.unavailable).count(),
        1,
        "the others are readable"
    );
}

/// A stored playlist of thousands is not a request per entry: every read
/// of it asked two of each, at any page, for its total.
#[tokio::test]
async fn a_large_stored_playlist_asks_for_its_entries_at_one_go() {
    let music = Big::new(Duration::ZERO, Duration::ZERO).answering_many();
    let h = harness("many-stored", music, |p| p).await;
    let id = h.stored(&(1..=3000).collect::<Vec<_>>()).await;
    let page = h.playlists.get(&h.alice, &id, 0, 100).await.unwrap();
    assert_eq!((page.total, page.playable), (3000, 2999));
    assert_eq!(page.entries.len(), 100);
    assert_eq!(h.music.files_alone.load(Ordering::SeqCst), 0);
    assert!(
        h.music.items_alone.load(Ordering::SeqCst) <= super::MOSAIC_LOOKUPS,
        "only its picture's"
    );
}

/// With All's list at hand, a page of a large stored playlist asks the
/// catalog of that page alone; a row is on the list, or it is resolved.
/// `playable` takes the files off the page for readable, as All's does:
/// only the track on the page is known not to be.
#[tokio::test]
async fn a_large_stored_playlist_asks_only_of_its_page_with_all_at_hand() {
    let music = Big::new(Duration::ZERO, Duration::ZERO).answering_many();
    let h = harness("many-page", music, |p| p).await;
    h.page(0).await;
    let id = h
        .stored_with_gone(&(1..=3000).collect::<Vec<_>>(), TRACKS + 1)
        .await;
    h.music.most_many.store(0, Ordering::SeqCst);
    let first = h.playlists.get(&h.alice, &id, 0, 100).await.unwrap();
    assert_eq!(h.music.most_many.load(Ordering::SeqCst), 100, "the page");
    assert_eq!(first.total, 3000, "its row dropped");
    assert_eq!(first.playable, 2999);
    assert_eq!(first.entries[2].item.id(), &plex_track(UNREADABLE));
    assert!(first.entries[2].unavailable);
    let second = h.playlists.get(&h.alice, &id, 100, 100).await.unwrap();
    assert_eq!(second.offset, 100);
    assert_eq!(second.entries[0].item.id(), &plex_track(1_000_101));
    assert_eq!(second.playable, 3000, "its page is all readable");
    assert_eq!(h.music.files_alone.load(Ordering::SeqCst), 0);
}

/// An entry the catalog no longer has still drops out, asked alone, and
/// the rest are not; the same track twice is two entries.
#[tokio::test]
async fn an_entry_left_out_of_the_answer_is_resolved_alone() {
    let music = Big::new(Duration::ZERO, Duration::ZERO).answering_many();
    let h = harness("many-gone", music, |p| p).await;
    let gone = TRACKS + 1;
    let id = h.stored_with_gone(&[1, 2, 2], gone).await;
    let page = h.playlists.get(&h.alice, &id, 0, 100).await.unwrap();
    let keys: Vec<&str> = page.entries.iter().map(|e| e.item.id().key()).collect();
    assert_eq!(keys, ["1000001", "1000002", "1000002"]);
    assert_eq!(page.total, 3);
    assert_eq!(
        h.playlists.store.playlist_entries(&id).await.unwrap().len(),
        3
    );
    assert_eq!(h.music.files_alone.load(Ordering::SeqCst), 0);
}

/// A scan shows in All once the list is older than it is kept for: the
/// read that finds it old starts the listing, and a read after has it.
#[tokio::test]
async fn a_scan_shows_in_all_after_the_list_is_kept_for() {
    let h = harness("scan", Big::new(Duration::ZERO, Duration::ZERO), |p| p).await;
    assert_eq!(h.page(0).await.total, 2 * TRACKS);
    h.music.add_track(2);
    assert_eq!(h.page(0).await.total, 2 * TRACKS, "kept a while");
    age_the_list().await;
    h.until_total(2 * TRACKS + 1).await;
}

/// The bug: past the time the list is kept, opening All waited for every
/// track of every library, 3 to 12 s on 16,000 tracks, and so did
/// everyone else reading All meanwhile.
#[tokio::test]
async fn an_old_list_is_read_while_the_libraries_are_listed_once_behind_it() {
    let h = harness("behind", Big::new(Duration::ZERO, Duration::ZERO), |p| p).await;
    h.page(0).await;
    let listed = h.listed();
    h.music.add_track(1);
    age_the_list().await;
    let held = h.music.listing.write().await;
    let (first, second) = tokio::join!(h.page_at_once(0), h.page_at_once(MAX_PAGE));
    assert_eq!(first.total, 2 * TRACKS, "the old list");
    assert_eq!(second.total, 2 * TRACKS, "the old list");
    h.page_at_once(0).await;
    assert_eq!(h.listed(), listed + 2, "each library listed once");
    drop(held);
    h.until_total(2 * TRACKS + 1).await;
}

/// A Plex server that is down now leaves the list there is.
#[tokio::test]
async fn an_old_list_is_read_while_the_libraries_cannot_be_listed() {
    let h = harness("down", Big::new(Duration::ZERO, Duration::ZERO), |p| p).await;
    h.page(0).await;
    h.music.down.store(true, Ordering::SeqCst);
    age_the_list().await;
    assert_eq!(h.page(0).await.total, 2 * TRACKS);
}

/// The first to open All after a start does not list the libraries: they
/// are listed ahead of it.
#[tokio::test]
async fn all_is_listed_ahead_of_its_first_read() {
    let h = harness("ahead", Big::new(Duration::ZERO, Duration::ZERO), |p| p).await;
    h.playlists.keep_all_listed();
    h.until_kept().await;
    let _held = h.music.listing.write().await;
    assert_eq!(h.page_at_once(0).await.total, 2 * TRACKS);
    assert_eq!(h.listed(), 2, "no read listed them");
}

/// A replaced catalog is listed with no read to ask for it.
#[tokio::test]
async fn a_replaced_catalog_lists_all_ahead_of_the_next_read() {
    let (replaced, changes) = tokio::sync::watch::channel(0u64);
    let h = harness(
        "ahead-replaced",
        Big::new(Duration::ZERO, Duration::ZERO),
        |p| p.with_catalog_changes(changes),
    )
    .await;
    h.playlists.keep_all_listed();
    h.until_listed(2).await;
    replaced.send_modify(|n| *n += 1);
    h.until_listed(4).await;
}

/// While a replaced catalog is listed, the list of the one before is not
/// read: what it served may be gone.
#[tokio::test]
async fn a_read_under_a_replaced_catalog_waits_for_its_list() {
    let (replaced, changes) = tokio::sync::watch::channel(0u64);
    let h = harness("waits", Big::new(Duration::ZERO, Duration::ZERO), |p| {
        p.with_catalog_changes(changes)
    })
    .await;
    h.playlists.keep_all_listed();
    h.until_total(2 * TRACKS).await;
    let held = h.music.listing.write().await;
    h.music.add_track(1);
    replaced.send_modify(|n| *n += 1);
    h.until_listed(4).await;
    let read = h.page(0);
    let waited = tokio::time::timeout(Duration::from_millis(50), read).await;
    assert!(waited.is_err(), "the replaced catalog's list was read");
    drop(held);
    assert_eq!(h.page(0).await.total, 2 * TRACKS + 1);
}

/// A replaced catalog (a library added, removed or changed) lists All
/// again at once.
#[tokio::test]
async fn a_replaced_catalog_lists_all_again_at_once() {
    let (replaced, changes) = tokio::sync::watch::channel(0u64);
    let h = harness("replaced", Big::new(Duration::ZERO, Duration::ZERO), |p| {
        p.with_catalog_changes(changes)
    })
    .await;
    assert_eq!(h.page(0).await.total, 2 * TRACKS);
    h.music.add_track(1);
    replaced.send_modify(|n| *n += 1);
    assert_eq!(h.page(0).await.total, 2 * TRACKS + 1);
}
