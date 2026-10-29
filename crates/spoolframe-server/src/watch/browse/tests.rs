//! A library sorted and filtered by the user's watch state, over a
//! catalog that leaves it to the server (as Plex does): the walk, the
//! filter and order by each user's own state, the pages, and the walk
//! kept and forgotten.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use spoolframe_catalog::{
    Browsing, Catalog, CatalogError, Found, Image, ItemPage, ItemQuery, ItemWithChildren, Listed,
    Page, SearchQuery, WatchParts,
};
use spoolframe_media::Prober;
use spoolframe_media::sandbox::Tool;
use spoolframe_types::{
    ArtworkId, FilterPart, Item, ItemFilter, ItemSort, Library, LibraryId, LibraryKind, MediaId,
    MediaKind, Movie, Namespace, PersonId, PlaybackState, Show, SortKey, WatchedState,
};

use super::super::{Watch, WatchRules};
use super::*;
use crate::principal::Principal;
use crate::store::Store;
use crate::store::watch::WatchUpdate;

/// A movie: its key, title, genre and year.
const FILMS: &[(&str, &str, &str, u16)] = &[
    ("1", "Alien", "Science Fiction", 1979),
    ("2", "Brazil", "Comedy", 1985),
    ("3", "Clue", "Comedy", 1985),
    ("4", "Dune", "Science Fiction", 2021),
    ("5", "Heat", "Drama", 1995),
    ("6", "Up", "Comedy", 2009),
];

/// A show: its key, title and how many episodes it has.
const SHOWS: &[(&str, &str, u32)] = &[
    ("10", "Andor", 3),
    ("20", "Bluey", 2),
    ("30", "Chernobyl", 1),
];

/// A catalog of one movie library (`plex:1`) and one TV library
/// (`plex:3`), sorted by title or year and filtered by genre in memory,
/// that leaves the watch state to the server.
#[derive(Default)]
struct Shelf {
    walks: AtomicUsize,
}

fn movie_id(key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, MediaKind::Movie, key).unwrap()
}

fn show_id(key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, MediaKind::Show, key).unwrap()
}

fn episode_id(key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, MediaKind::Episode, key).unwrap()
}

fn movies() -> Library {
    Library {
        id: LibraryId::new(Namespace::Plex, "1").unwrap(),
        title: "Movies".into(),
        kind: LibraryKind::Movies,
        server: None,
    }
}

fn tv() -> Library {
    Library {
        id: LibraryId::new(Namespace::Plex, "3").unwrap(),
        title: "TV".into(),
        kind: LibraryKind::Shows,
        server: None,
    }
}

fn film(key: &str) -> Item {
    let (_, title, _, year) = FILMS.iter().find(|f| f.0 == key).unwrap();
    Item::Movie(Movie {
        id: movie_id(key),
        title: (*title).into(),
        year: Some(*year),
        release_date: None,
        summary: None,
        content_rating: None,
        duration: None,
        cast: Vec::new(),
        artwork: Vec::new(),
        playback: PlaybackState::default(),
    })
}

fn series(key: &str) -> Item {
    let (_, title, _) = SHOWS.iter().find(|s| s.0 == key).unwrap();
    Item::Show(Show {
        id: show_id(key),
        title: (*title).into(),
        year: None,
        summary: None,
        content_rating: None,
        season_count: None,
        genres: Vec::new(),
        last_aired: None,
        cast: Vec::new(),
        artwork: Vec::new(),
    })
}

impl Shelf {
    /// `query`'s items of `library`, in its order; panics at a watch
    /// state, which a catalog that leaves it to the server is never asked.
    fn listed(&self, library: &LibraryId, query: &ItemQuery) -> Vec<Listed> {
        assert!(
            !query.needs_viewer(),
            "asked for the watch state: {query:?}"
        );
        if library.key() == "3" {
            return SHOWS
                .iter()
                .map(|(key, _, leaves)| Listed {
                    id: show_id(key),
                    leaves: Some(*leaves),
                })
                .collect();
        }
        let mut films: Vec<_> = FILMS
            .iter()
            .filter(|(_, _, genre, _)| {
                query.filter.genres.is_empty()
                    || query
                        .filter
                        .genres
                        .iter()
                        .any(|g| g.eq_ignore_ascii_case(genre))
            })
            .collect();
        match query.sort.key {
            SortKey::Title => films.sort_by_key(|f| f.1),
            SortKey::Released => films.sort_by_key(|f| (f.3, f.1)),
            other => panic!("the shelf does not sort by {other:?}"),
        }
        if query.sort.descending {
            films.reverse();
        }
        films
            .into_iter()
            .map(|(key, ..)| Listed {
                id: movie_id(key),
                leaves: None,
            })
            .collect()
    }
}

#[async_trait]
impl Catalog for Shelf {
    async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
        Ok(vec![movies(), tv()])
    }

    /// The plain order, as Plex lists a section without a sort: by key,
    /// not by title, so a test tells the two apart.
    async fn items(&self, library: &LibraryId, page: Page) -> Result<ItemPage, CatalogError> {
        let all: Vec<Item> = if library.key() == "3" {
            SHOWS.iter().map(|s| series(s.0)).collect()
        } else {
            FILMS.iter().rev().map(|f| film(f.0)).collect()
        };
        let total = all.len() as u32;
        Ok(ItemPage {
            items: all
                .into_iter()
                .skip(page.offset as usize)
                .take(page.limit as usize)
                .collect(),
            total,
        })
    }

    async fn browsing(&self, library: &Library) -> Result<Browsing, CatalogError> {
        Ok(Browsing {
            sorts: vec![SortKey::Title, SortKey::Released, SortKey::LastPlayed],
            filters: if library.kind == LibraryKind::Movies {
                vec![FilterPart::Watched, FilterPart::Genres]
            } else {
                vec![FilterPart::Watched]
            },
            dynamic_ranges: Vec::new(),
            watch: WatchParts::ByServer,
        })
    }

    async fn query_items(
        &self,
        library: &LibraryId,
        query: &ItemQuery,
        page: Page,
    ) -> Result<ItemPage, CatalogError> {
        let listed = self.listed(library, query);
        let total = listed.len() as u32;
        let ids: Vec<MediaId> = listed
            .into_iter()
            .skip(page.offset as usize)
            .take(page.limit as usize)
            .map(|l| l.id)
            .collect();
        Ok(ItemPage {
            items: self.items_by_id(&ids).await?,
            total,
        })
    }

    async fn walk_items(
        &self,
        library: &LibraryId,
        query: &ItemQuery,
    ) -> Result<Vec<Listed>, CatalogError> {
        self.walks.fetch_add(1, Ordering::SeqCst);
        Ok(self.listed(library, query))
    }

    async fn items_by_id(&self, ids: &[MediaId]) -> Result<Vec<Item>, CatalogError> {
        Ok(ids
            .iter()
            .map(|id| match id.kind() {
                MediaKind::Show => series(id.key()),
                _ => film(id.key()),
            })
            .collect())
    }

    async fn item(&self, id: &MediaId) -> Result<ItemWithChildren, CatalogError> {
        if id.kind() != MediaKind::Movie || !FILMS.iter().any(|f| f.0 == id.key()) {
            return Err(CatalogError::NotFound(id.to_string()));
        }
        Ok(ItemWithChildren {
            item: film(id.key()),
            children: Vec::new(),
            external_ids: Vec::new(),
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

    async fn media_path(&self, id: &MediaId) -> Result<PathBuf, CatalogError> {
        Err(CatalogError::NotFound(id.to_string()))
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
    watch: Arc<Watch>,
    store: Store,
    shelf: Arc<Shelf>,
    alice: Principal,
    bob: Principal,
    _dir: spoolframe_test_dir::TestDir,
}

async fn harness(test: &str) -> Harness {
    let dir = crate::store::tests::state_dir(&format!("browse-{test}")).await;
    let store = Store::open(dir.as_ref()).await.unwrap();
    let shelf = Arc::new(Shelf::default());
    let prober = Arc::new(Prober::new(
        Tool::unconfined(dir.join("no-ffprobe")),
        Prober::DEFAULT_TIMEOUT,
    ));
    let watch = Arc::new(Watch::new(
        store.clone(),
        shelf.clone(),
        prober,
        WatchRules::default(),
    ));
    let mut users = Vec::new();
    for name in ["alice", "bob"] {
        let id = store
            .add_user(name, None, "phc", spoolframe_types::Role::Consumer)
            .await
            .unwrap();
        users.push(Principal::LocalUser(id));
    }
    Harness {
        watch,
        store,
        shelf,
        alice: users[0],
        bob: users[1],
        _dir: dir,
    }
}

/// A state, as of `at`.
#[derive(Clone, Copy)]
enum State {
    Watched,
    /// Ten minutes in.
    Started,
    /// Ten seconds in: not counted as started.
    Glanced,
}

impl Harness {
    async fn saw(
        &self,
        who: Principal,
        id: &MediaId,
        show: Option<&MediaId>,
        state: State,
        at: i64,
    ) {
        let show = show.map(MediaId::to_string);
        let (position_ms, watched) = match state {
            State::Watched => (None, true),
            State::Started => (Some(600_000), false),
            State::Glanced => (Some(10_000), false),
        };
        self.store
            .save_watch_at(
                who,
                WatchUpdate {
                    media_id: &id.to_string(),
                    show_id: show.as_deref(),
                    external_ids: &[],
                    position_ms,
                    duration_ms: Some(7_200_000),
                    watched,
                    unhide: false,
                },
                at,
            )
            .await
            .unwrap();
    }

    async fn page(
        &self,
        who: Principal,
        library: &Library,
        sort: Option<ItemSort>,
        filter: Option<ItemFilter>,
        page: Page,
    ) -> Result<(Vec<String>, u32), WatchError> {
        let got = self.watch.browse(who, library, sort, filter, page).await?;
        Ok((
            got.items.iter().map(|i| i.id().key().to_owned()).collect(),
            got.total,
        ))
    }

    async fn all(
        &self,
        who: Principal,
        library: &Library,
        sort: Option<ItemSort>,
        filter: Option<ItemFilter>,
    ) -> Vec<String> {
        let (keys, total) = self
            .page(
                who,
                library,
                sort,
                filter,
                Page {
                    offset: 0,
                    limit: 100,
                },
            )
            .await
            .unwrap();
        assert_eq!(total as usize, keys.len());
        keys
    }
}

fn watched(states: &[WatchedState]) -> Option<ItemFilter> {
    Some(ItemFilter {
        watched: states.to_vec(),
        ..ItemFilter::default()
    })
}

fn sort(key: SortKey, descending: bool) -> Option<ItemSort> {
    Some(ItemSort { key, descending })
}

/// Alice watched Alien, started Clue, glanced at Heat; Bob watched Up.
async fn alice_and_bob(h: &Harness) {
    h.saw(h.alice, &movie_id("1"), None, State::Watched, 100)
        .await;
    h.saw(h.alice, &movie_id("3"), None, State::Started, 300)
        .await;
    h.saw(h.alice, &movie_id("5"), None, State::Glanced, 200)
        .await;
    h.saw(h.bob, &movie_id("6"), None, State::Watched, 400)
        .await;
}

#[tokio::test]
async fn each_watch_state_is_the_user_s_own() {
    let h = harness("own-state").await;
    alice_and_bob(&h).await;
    use WatchedState::*;
    assert_eq!(
        h.all(h.alice, &movies(), None, watched(&[Watched])).await,
        ["1"]
    );
    assert_eq!(
        h.all(h.alice, &movies(), None, watched(&[InProgress]))
            .await,
        ["3"]
    );
    // Heat, glanced at, is not started.
    assert_eq!(
        h.all(h.alice, &movies(), None, watched(&[Unwatched])).await,
        ["2", "4", "5", "6"]
    );
    assert_eq!(
        h.all(h.alice, &movies(), None, watched(&[Watched, InProgress]))
            .await,
        ["1", "3"]
    );
    // Bob's watched is not Alice's.
    assert_eq!(
        h.all(h.bob, &movies(), None, watched(&[Watched])).await,
        ["6"]
    );
    assert_eq!(
        h.all(h.bob, &movies(), None, watched(&[Unwatched])).await,
        ["1", "2", "3", "4", "5"]
    );
}

#[tokio::test]
async fn last_played_puts_the_never_played_last_both_ways() {
    let h = harness("last-played").await;
    alice_and_bob(&h).await;
    assert_eq!(
        h.all(h.alice, &movies(), sort(SortKey::LastPlayed, true), None)
            .await,
        ["3", "5", "1", "2", "4", "6"]
    );
    assert_eq!(
        h.all(h.alice, &movies(), sort(SortKey::LastPlayed, false), None)
            .await,
        ["1", "5", "3", "2", "4", "6"]
    );
    // Bob's plays order Bob's list.
    assert_eq!(
        h.all(h.bob, &movies(), sort(SortKey::LastPlayed, true), None)
            .await,
        ["6", "1", "2", "3", "4", "5"]
    );
}

#[tokio::test]
async fn the_watch_state_combines_with_the_catalog_s_sort_and_filter() {
    let h = harness("combined").await;
    alice_and_bob(&h).await;
    let unwatched_comedies = Some(ItemFilter {
        watched: vec![WatchedState::Unwatched],
        genres: vec!["comedy".into()],
        ..ItemFilter::default()
    });
    assert_eq!(
        h.all(
            h.alice,
            &movies(),
            sort(SortKey::Released, true),
            unwatched_comedies
        )
        .await,
        ["6", "2"]
    );
    let comedies = Some(ItemFilter {
        genres: vec!["Comedy".into()],
        ..ItemFilter::default()
    });
    assert_eq!(
        h.all(
            h.alice,
            &movies(),
            sort(SortKey::LastPlayed, true),
            comedies
        )
        .await,
        ["3", "2", "6"]
    );
}

#[tokio::test]
async fn a_show_is_watched_when_every_episode_is() {
    let h = harness("shows").await;
    let andor = show_id("10");
    let bluey = show_id("20");
    // Andor: two of three watched. Bluey: both. Chernobyl: none.
    h.saw(
        h.alice,
        &episode_id("11"),
        Some(&andor),
        State::Watched,
        100,
    )
    .await;
    h.saw(
        h.alice,
        &episode_id("12"),
        Some(&andor),
        State::Watched,
        500,
    )
    .await;
    h.saw(
        h.alice,
        &episode_id("21"),
        Some(&bluey),
        State::Watched,
        200,
    )
    .await;
    h.saw(
        h.alice,
        &episode_id("22"),
        Some(&bluey),
        State::Watched,
        300,
    )
    .await;
    use WatchedState::*;
    assert_eq!(
        h.all(h.alice, &tv(), None, watched(&[Watched])).await,
        ["20"]
    );
    assert_eq!(
        h.all(h.alice, &tv(), None, watched(&[InProgress])).await,
        ["10"]
    );
    assert_eq!(
        h.all(h.alice, &tv(), None, watched(&[Unwatched])).await,
        ["30"]
    );
    // By its latest episode: Andor at 500, Bluey at 300.
    assert_eq!(
        h.all(h.alice, &tv(), sort(SortKey::LastPlayed, true), None)
            .await,
        ["10", "20", "30"]
    );
    // Bob has seen none of it.
    assert_eq!(
        h.all(h.bob, &tv(), None, watched(&[Unwatched])).await,
        ["10", "20", "30"]
    );
}

#[tokio::test]
async fn pages_with_ties_never_repeat_or_skip_and_walk_once() {
    let h = harness("pages").await;
    // Three played at the same moment: ties, in title order.
    for key in ["4", "2", "6"] {
        h.saw(h.alice, &movie_id(key), None, State::Started, 100)
            .await;
    }
    let order = sort(SortKey::LastPlayed, true);
    let whole = h.all(h.alice, &movies(), order, None).await;
    assert_eq!(whole, ["2", "4", "6", "1", "3", "5"]);
    let walks = h.shelf.walks.load(Ordering::SeqCst);
    let mut paged = Vec::new();
    for offset in (0..6).step_by(4) {
        let (keys, total) = h
            .page(h.alice, &movies(), order, None, Page { offset, limit: 4 })
            .await
            .unwrap();
        assert_eq!(total, 6);
        paged.extend(keys);
    }
    assert_eq!(paged, whole);
    assert_eq!(
        h.shelf.walks.load(Ordering::SeqCst),
        walks,
        "the pages after the first use the walk kept"
    );
    let (past, total) = h
        .page(
            h.alice,
            &movies(),
            order,
            None,
            Page {
                offset: 6,
                limit: 4,
            },
        )
        .await
        .unwrap();
    assert_eq!((past.len(), total), (0, 6));
}

#[tokio::test]
async fn a_change_of_the_user_s_state_is_seen_at_once() {
    let h = harness("forget").await;
    let unwatched = watched(&[WatchedState::Unwatched]);
    assert_eq!(
        h.all(h.alice, &movies(), None, unwatched.clone())
            .await
            .len(),
        6
    );
    h.saw(h.alice, &movie_id("1"), None, State::Watched, 100)
        .await;
    // Saved straight to the store: the walk kept still says six.
    assert_eq!(
        h.all(h.alice, &movies(), None, unwatched.clone())
            .await
            .len(),
        6
    );
    // Saved through the watch state, as a client's mark is: the walk is
    // forgotten, and both are seen.
    h.watch
        .mark(h.alice, &movie_id("2"), true, None)
        .await
        .unwrap();
    assert_eq!(
        h.all(h.alice, &movies(), None, unwatched.clone()).await,
        ["3", "4", "5", "6"]
    );
    // Bob's walk is his own, and stays.
    let bob_s = h.all(h.bob, &movies(), None, unwatched.clone()).await;
    h.watch
        .mark(h.alice, &movie_id("3"), true, None)
        .await
        .unwrap();
    let walks = h.shelf.walks.load(Ordering::SeqCst);
    assert_eq!(h.all(h.bob, &movies(), None, unwatched).await, bob_s);
    assert_eq!(h.shelf.walks.load(Ordering::SeqCst), walks);
}

#[tokio::test]
async fn without_a_sort_or_filter_the_plain_order_is_kept() {
    let h = harness("plain").await;
    assert_eq!(
        h.all(h.alice, &movies(), None, None).await,
        ["6", "5", "4", "3", "2", "1"],
        "an older client's order, as the catalog lists it"
    );
    assert_eq!(h.shelf.walks.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn what_the_library_does_not_offer_is_refused() {
    let h = harness("refused").await;
    let refused = |r: Result<(Vec<String>, u32), WatchError>| {
        matches!(r, Err(WatchError::Catalog(CatalogError::CannotBrowse(_))))
    };
    let all = Page {
        offset: 0,
        limit: 10,
    };
    // Not offered: a size sort, a genre in TV.
    assert!(refused(
        h.page(h.alice, &movies(), sort(SortKey::Size, false), None, all)
            .await
    ));
    let genre = Some(ItemFilter {
        genres: vec!["Drama".into()],
        ..ItemFilter::default()
    });
    assert!(refused(h.page(h.alice, &tv(), None, genre, all).await));
    // Not known to this build: a newer client's key or state.
    assert!(refused(
        h.page(h.alice, &movies(), sort(SortKey::Unknown, false), None, all)
            .await
    ));
    assert!(refused(
        h.page(
            h.alice,
            &movies(),
            None,
            watched(&[WatchedState::Unknown]),
            all
        )
        .await
    ));
    let backwards = Some(ItemFilter {
        year_from: Some(2000),
        year_to: Some(1990),
        ..ItemFilter::default()
    });
    assert!(refused(
        h.page(h.alice, &movies(), None, backwards, all).await
    ));
}

#[test]
fn the_walks_kept_hold_at_most_so_many_ids_the_oldest_going_first() {
    let walks = Walks {
        hold: 5,
        ..Walks::default()
    };
    let principal = Principal::LocalUser(crate::store::users::LocalUserId::generate());
    let key = |n: &str| WalkKey {
        principal,
        library: n.into(),
        sort: ItemSort::DEFAULT,
        filter: ItemFilter::default(),
    };
    let ids = |n: usize| Arc::new((1..=n).map(|k| movie_id(&k.to_string())).collect());
    walks.keep(key("a"), ids(2));
    walks.keep(key("b"), ids(2));
    assert_eq!(walks.len(), 2);
    walks.keep(key("c"), ids(3));
    assert!(walks.get(&key("a")).is_none(), "the oldest went");
    assert!(walks.get(&key("b")).is_some());
    assert!(walks.get(&key("c")).is_some());
    walks.keep(key("d"), ids(6));
    assert!(walks.get(&key("d")).is_none(), "too large alone: not kept");
}
