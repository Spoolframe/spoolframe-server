//! All (PX-43) on a large music library: two libraries of 10,000 tracks
//! each, listed at once as the Plex catalog lists them, with files that
//! take a while to look at.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use spoolframe_catalog::{
    Catalog, CatalogError, Found, Image, ItemPage, ItemWithChildren, Page, SearchQuery,
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
    /// `tracks` calls so far.
    listed: AtomicUsize,
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
            listed: AtomicUsize::new(0),
            looking: AtomicUsize::new(0),
            most_looking: AtomicUsize::new(0),
        }
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
        let n: u32 = library.key().parse().unwrap();
        let size = self.sizes.lock()[n as usize - 1];
        Ok((1..=size).map(|t| Self::track(n, t)).collect())
    }

    async fn item(&self, id: &MediaId) -> Result<ItemWithChildren, CatalogError> {
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
        let now = self.looking.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_looking.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(self.file_takes).await;
        self.looking.fetch_sub(1, Ordering::SeqCst);
        Ok(PathBuf::from(format!("/music/{}.flac", id.key())))
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

/// A scan shows in All once the list is older than it is kept for.
/// Tokio's clock is moved on, not slept through: a slow machine can't
/// age the list between the two reads that must share it.
#[tokio::test]
async fn a_scan_shows_in_all_after_the_list_is_kept_for() {
    let h = harness("scan", Big::new(Duration::ZERO, Duration::ZERO), |p| p).await;
    assert_eq!(h.page(0).await.total, 2 * TRACKS);
    h.music.add_track(2);
    assert_eq!(h.page(0).await.total, 2 * TRACKS, "kept a while");
    tokio::time::pause();
    tokio::time::advance(super::ALL_KEPT_FOR).await;
    tokio::time::resume();
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
