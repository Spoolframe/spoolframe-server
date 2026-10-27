//! A paired server's library sorted and filtered in the cache: every key
//! both ways, every filter part, the two together, pages with ties, and
//! each user's own watch state, through [`PeerCache`] as the client
//! protocol asks it; and what a library has to filter by.

use std::cmp::Ordering;

use spoolframe_catalog::{Catalog, CatalogError, ItemQuery, Page, Viewer};
use spoolframe_protocol::federation::{RemoteKind, RemoteLibraryKind, SharedLibrary};
use spoolframe_types::{
    DynamicRange, FilterPart, ItemFilter, ItemSort, Library, LibraryId, LibraryKind, MediaId,
    Resolution, SortKey, WatchedState,
};

use super::*;
use crate::principal::Principal;
use crate::store::federation::NewPeer;
use crate::store::federation_cache::{CacheSynced, OriginLimits};
use crate::store::tests::{TestStore, state_dir, test_store};
use crate::store::watch::WatchUpdate;

const ORIGIN: &str = "aaaaaaaaaaaaaaaa";

/// One film of the cached Movies: its key, then what it is sorted and
/// filtered by.
struct Film {
    key: &'static str,
    title: &'static str,
    sort_title: Option<&'static str>,
    released: Option<&'static str>,
    added: i64,
    rating: Option<u16>,
    audience: Option<u16>,
    duration: Option<u64>,
    size: Option<u64>,
    resolution: Option<Resolution>,
    range: Option<DynamicRange>,
    genres: &'static [&'static str],
    content: &'static str,
    year: u16,
}

const G: u64 = 1 << 30;

fn films() -> Vec<Film> {
    use DynamicRange::*;
    use Resolution::*;
    let film = |key, title, year| Film {
        key,
        title,
        sort_title: None,
        released: None,
        added: 0,
        rating: None,
        audience: None,
        duration: None,
        size: None,
        resolution: None,
        range: None,
        genres: &[],
        content: "R",
        year,
    };
    vec![
        Film {
            sort_title: Some("Matrix"),
            released: Some("1999-03-31"),
            added: 100,
            rating: Some(87),
            audience: Some(85),
            duration: Some(8_160_000),
            size: Some(30 * G),
            resolution: Some(FullHd),
            range: Some(Sdr),
            genres: &["Action", "Science Fiction"],
            ..film("1", "The Matrix", 1999)
        },
        Film {
            released: Some("1979-05-25"),
            added: 200,
            rating: Some(98),
            audience: Some(94),
            duration: Some(7_020_000),
            size: Some(50 * G),
            resolution: Some(Uhd),
            range: Some(Hdr),
            genres: &["Science Fiction", "Horror"],
            ..film("2", "Alien", 1979)
        },
        Film {
            released: Some("1986-07-18"),
            added: 300,
            audience: Some(94),
            duration: Some(8_220_000),
            size: Some(40 * G),
            resolution: Some(Uhd),
            range: Some(DolbyVision),
            genres: &["Action", "Science Fiction"],
            ..film("3", "Aliens", 1986)
        },
        Film {
            released: Some("1995-12-15"),
            added: 300,
            rating: Some(87),
            duration: Some(10_200_000),
            resolution: Some(FullHd),
            genres: &["Crime", "Drama"],
            ..film("4", "Heat", 1995)
        },
        Film {
            released: Some("2016-11-11"),
            added: 500,
            rating: Some(94),
            audience: Some(82),
            duration: Some(6_960_000),
            size: Some(20 * G),
            resolution: Some(Uhd),
            range: Some(DolbyVision),
            genres: &["Drama", "Science Fiction"],
            content: "PG-13",
            ..film("5", "Arrival", 2016)
        },
        Film {
            added: 600,
            rating: Some(98),
            audience: Some(90),
            duration: Some(5_760_000),
            size: Some(8 * G),
            resolution: Some(Hd),
            range: Some(Sdr),
            genres: &["Comedy", "Animation"],
            content: "PG",
            ..film("6", "Up", 2009)
        },
        Film {
            released: Some("1985-12-20"),
            added: 700,
            size: Some(3 * G),
            resolution: Some(Sd),
            range: Some(Sdr),
            genres: &["Comedy"],
            ..film("7", "Brazil", 1985)
        },
        Film {
            released: Some("1996-12-25"),
            added: 800,
            rating: Some(70),
            duration: Some(14_520_000),
            size: Some(25 * G),
            resolution: Some(FullHd),
            range: Some(Sdr),
            genres: &["Drama"],
            content: "PG-13",
            ..film("8", "Hamlet", 1996)
        },
        Film {
            released: Some("1990-12-19"),
            added: 800,
            rating: Some(70),
            duration: Some(8_100_000),
            size: Some(12 * G),
            resolution: Some(FullHd),
            range: Some(Sdr),
            genres: &["Drama"],
            content: "PG",
            ..film("9", "Hamlet", 1990)
        },
    ]
}

fn blank(id: &str, kind: RemoteKind, title: &str) -> RemoteItem {
    RemoteItem {
        id: id.into(),
        kind,
        title: title.into(),
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
        sort_title: None,
        released: None,
        added_at: None,
        rating_tenths: None,
        audience_rating_tenths: None,
        size: None,
        resolution: None,
        dynamic_range: None,
        hash: String::new(),
    }
}

impl Film {
    fn remote(&self) -> RemoteItem {
        RemoteItem {
            sort_title: self.sort_title.map(str::to_owned),
            released: self.released.map(str::to_owned),
            added_at: Some(self.added),
            rating_tenths: self.rating,
            audience_rating_tenths: self.audience,
            duration_ms: self.duration,
            size: self.size,
            resolution: self.resolution,
            dynamic_range: self.range,
            genres: self.genres.iter().map(|g| (*g).to_owned()).collect(),
            content_rating: Some(self.content.into()),
            year: Some(self.year),
            ..blank(
                &format!("plex:movie:{}", self.key),
                RemoteKind::Movie,
                self.title,
            )
        }
        .hashed()
    }

    fn sort_title(&self) -> String {
        self.sort_title
            .unwrap_or_else(|| spoolframe_types::browse::sort_title(self.title))
            .to_lowercase()
    }

    /// What `key` orders it by, as text that orders as the value does.
    fn value(&self, key: SortKey) -> Option<String> {
        match key {
            SortKey::Title => Some(self.sort_title()),
            SortKey::Released => self.released.map(str::to_owned),
            SortKey::Added => Some(format!("{:012}", self.added)),
            SortKey::Rating => self.rating.map(|r| format!("{r:04}")),
            SortKey::AudienceRating => self.audience.map(|r| format!("{r:04}")),
            SortKey::Duration => self.duration.map(|d| format!("{d:015}")),
            SortKey::Size => self.size.map(|s| format!("{s:020}")),
            other => panic!("{other:?} is not the film's own"),
        }
    }
}

/// The films' keys by `key`: those with a value in order, those without
/// last both ways, ties by sort title then id.
fn expected(key: SortKey, descending: bool) -> Vec<&'static str> {
    let mut all = films();
    all.sort_by(|a, b| {
        let by_key = match (a.value(key), b.value(key)) {
            (Some(x), Some(y)) if descending => y.cmp(&x),
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        by_key
            .then_with(|| a.sort_title().cmp(&b.sort_title()))
            .then_with(|| a.key.cmp(b.key))
    });
    all.iter().map(|f| f.key).collect()
}

struct Harness {
    cache: PeerCache,
    store: TestStore,
    alice: Principal,
    bob: Principal,
}

fn fed(id: &str) -> MediaId {
    MediaId::federated(ORIGIN, &id.parse().unwrap()).unwrap()
}

fn movies() -> Library {
    Library {
        id: LibraryId::federated(ORIGIN, &"plex:1".parse().unwrap()).unwrap(),
        title: "Movies".into(),
        kind: LibraryKind::Movies,
        server: None,
    }
}

fn shows() -> Library {
    Library {
        id: LibraryId::federated(ORIGIN, &"plex:2".parse().unwrap()).unwrap(),
        title: "Shows".into(),
        kind: LibraryKind::Shows,
        server: None,
    }
}

/// Andor (three episodes), Bluey (two) and Chernobyl (one), in Shows.
fn tv() -> Vec<RemoteItem> {
    let mut items = Vec::new();
    for (show, title, episodes) in [("1", "Andor", 3), ("2", "Bluey", 2), ("3", "Chernobyl", 1)] {
        let show_id = format!("plex:show:{show}");
        items.push(blank(&show_id, RemoteKind::Show, title).hashed());
        for n in 0..episodes {
            items.push(
                RemoteItem {
                    grandparent: Some(show_id.clone()),
                    index: Some(n + 1),
                    ..blank(
                        &format!("plex:episode:{show}{n}"),
                        RemoteKind::Episode,
                        &format!("{title} {n}"),
                    )
                }
                .hashed(),
            );
        }
    }
    items
}

async fn harness(test: &str) -> Harness {
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
    let films: Vec<RemoteItem> = films().iter().map(Film::remote).collect();
    for (library, title, kind, name, items) in [
        (
            "plex:1",
            "Movies",
            RemoteLibraryKind::Movies,
            "movies",
            films,
        ),
        ("plex:2", "Shows", RemoteLibraryKind::Shows, "shows", tv()),
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
    let mut users = Vec::new();
    for name in ["alice", "bob"] {
        let id = store
            .add_user(name, None, "phc", spoolframe_types::Role::Consumer)
            .await
            .unwrap();
        users.push(Principal::LocalUser(id));
    }
    let dir = state_dir(&format!("{test}-art")).await;
    let cache = PeerCache::new(
        store.store.clone(),
        dir.as_ref().join("peers"),
        FileLimits {
            per_origin: u64::MAX,
            total: u64::MAX,
        },
        Arc::new(Links::default()),
    );
    Harness {
        cache,
        store,
        alice: users[0],
        bob: users[1],
    }
}

/// A state, as of `at`.
#[derive(Clone, Copy)]
enum State {
    Watched,
    /// Ten minutes in.
    Started,
}

impl Harness {
    async fn saw(&self, who: Principal, id: &str, show: Option<&str>, state: State, at: i64) {
        let media = fed(id).to_string();
        let show = show.map(|s| fed(s).to_string());
        let (position_ms, watched) = match state {
            State::Watched => (None, true),
            State::Started => (Some(600_000), false),
        };
        self.store
            .save_watch_at(
                who,
                WatchUpdate {
                    media_id: &media,
                    show_id: show.as_deref(),
                    external_ids: &[],
                    position_ms,
                    duration_ms: None,
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
        library: &Library,
        who: Option<Principal>,
        sort: ItemSort,
        filter: ItemFilter,
        page: Page,
    ) -> (Vec<String>, u32) {
        let query = ItemQuery {
            sort,
            filter,
            viewer: who.map(|p| Viewer {
                principal: p.to_string(),
                started_after_ms: 60_000,
            }),
        };
        let got = self
            .cache
            .query_items(&library.id, &query, page)
            .await
            .unwrap();
        let keys = got
            .items
            .iter()
            .map(|i| {
                let (_, remote) = i.id().remote().unwrap();
                remote.key().to_owned()
            })
            .collect();
        (keys, got.total)
    }

    async fn all(
        &self,
        library: &Library,
        who: Option<Principal>,
        sort: ItemSort,
        filter: ItemFilter,
    ) -> Vec<String> {
        let (keys, total) = self
            .page(
                library,
                who,
                sort,
                filter,
                Page {
                    offset: 0,
                    limit: 100,
                },
            )
            .await;
        assert_eq!(total as usize, keys.len());
        keys
    }
}

fn by(key: SortKey, descending: bool) -> ItemSort {
    ItemSort { key, descending }
}

const FILM_KEYS: [SortKey; 7] = [
    SortKey::Title,
    SortKey::Released,
    SortKey::Added,
    SortKey::Rating,
    SortKey::AudienceRating,
    SortKey::Duration,
    SortKey::Size,
];

#[tokio::test]
async fn every_key_sorts_both_ways_with_no_value_last() {
    let h = harness("fed-browse-sorts").await;
    for key in FILM_KEYS {
        for descending in [false, true] {
            assert_eq!(
                h.all(&movies(), None, by(key, descending), ItemFilter::default())
                    .await,
                expected(key, descending),
                "{key:?} descending {descending}"
            );
        }
    }
}

#[tokio::test]
async fn title_sets_a_leading_article_aside() {
    let h = harness("fed-browse-title").await;
    assert_eq!(
        h.all(
            &movies(),
            None,
            by(SortKey::Title, false),
            ItemFilter::default()
        )
        .await,
        ["2", "3", "5", "7", "8", "9", "4", "1", "6"]
    );
}

#[tokio::test]
async fn pages_with_ties_never_repeat_or_skip() {
    let h = harness("fed-browse-pages").await;
    for key in FILM_KEYS {
        for descending in [false, true] {
            let whole = h
                .all(&movies(), None, by(key, descending), ItemFilter::default())
                .await;
            let mut paged = Vec::new();
            for offset in (0..12).step_by(2) {
                let (keys, total) = h
                    .page(
                        &movies(),
                        None,
                        by(key, descending),
                        ItemFilter::default(),
                        Page { offset, limit: 2 },
                    )
                    .await;
                assert_eq!(total, 9);
                paged.extend(keys);
            }
            assert_eq!(paged, whole, "{key:?} {descending}");
        }
    }
}

async fn kept(h: &Harness, filter: ItemFilter) -> Vec<String> {
    let mut keys = h.all(&movies(), None, ItemSort::DEFAULT, filter).await;
    keys.sort();
    keys
}

#[tokio::test]
async fn every_filter_part_keeps_what_it_names() {
    let h = harness("fed-browse-filters").await;
    let genres = ItemFilter {
        genres: vec!["comedy".into(), "HORROR".into()],
        ..ItemFilter::default()
    };
    assert_eq!(kept(&h, genres).await, ["2", "6", "7"]);
    let decade = ItemFilter {
        year_from: Some(1990),
        year_to: Some(1999),
        ..ItemFilter::default()
    };
    assert_eq!(kept(&h, decade).await, ["1", "4", "8", "9"]);
    let rated = ItemFilter {
        content_ratings: vec!["pg".into(), "PG-13".into()],
        ..ItemFilter::default()
    };
    assert_eq!(kept(&h, rated).await, ["5", "6", "8", "9"]);
    let uhd = ItemFilter {
        resolutions: vec![Resolution::Uhd, Resolution::Sd],
        ..ItemFilter::default()
    };
    assert_eq!(kept(&h, uhd).await, ["2", "3", "5", "7"]);
    let ranges = |r: &[DynamicRange]| ItemFilter {
        dynamic_ranges: r.to_vec(),
        ..ItemFilter::default()
    };
    // HDR takes in Dolby Vision; Heat, with no range known, is in none.
    assert_eq!(
        kept(&h, ranges(&[DynamicRange::Hdr])).await,
        ["2", "3", "5"]
    );
    assert_eq!(
        kept(&h, ranges(&[DynamicRange::DolbyVision])).await,
        ["3", "5"]
    );
    assert_eq!(
        kept(&h, ranges(&[DynamicRange::Sdr])).await,
        ["1", "6", "7", "8", "9"]
    );
    assert_eq!(
        kept(
            &h,
            ItemFilter {
                genres: vec!["Western".into()],
                ..ItemFilter::default()
            }
        )
        .await,
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn parts_combine_with_and_and_sort_together() {
    let h = harness("fed-browse-combined").await;
    let filter = ItemFilter {
        genres: vec!["Science Fiction".into()],
        resolutions: vec![Resolution::Uhd],
        year_from: Some(1980),
        ..ItemFilter::default()
    };
    assert_eq!(
        h.all(&movies(), None, by(SortKey::Size, true), filter)
            .await,
        ["3", "5"]
    );
}

#[tokio::test]
async fn the_watch_state_is_the_user_s_own() {
    let h = harness("fed-browse-watched").await;
    h.saw(h.alice, "plex:movie:1", None, State::Watched, 100)
        .await;
    h.saw(h.alice, "plex:movie:3", None, State::Started, 300)
        .await;
    h.saw(h.alice, "plex:movie:5", None, State::Watched, 200)
        .await;
    h.saw(h.bob, "plex:movie:2", None, State::Watched, 400)
        .await;
    let watched = |s: &[WatchedState]| ItemFilter {
        watched: s.to_vec(),
        ..ItemFilter::default()
    };
    let title = ItemSort::DEFAULT;
    use WatchedState::*;
    assert_eq!(
        h.all(&movies(), Some(h.alice), title, watched(&[Watched]))
            .await,
        ["5", "1"]
    );
    assert_eq!(
        h.all(&movies(), Some(h.alice), title, watched(&[InProgress]))
            .await,
        ["3"]
    );
    assert_eq!(
        h.all(&movies(), Some(h.alice), title, watched(&[Unwatched]))
            .await,
        ["2", "7", "8", "9", "4", "6"]
    );
    assert_eq!(
        h.all(&movies(), Some(h.bob), title, watched(&[Watched]))
            .await,
        ["2"]
    );
    // Last played: the played by when, the rest after, by title.
    assert_eq!(
        h.all(
            &movies(),
            Some(h.alice),
            by(SortKey::LastPlayed, true),
            ItemFilter::default()
        )
        .await,
        ["3", "5", "1", "2", "7", "8", "9", "4", "6"]
    );
    assert_eq!(
        h.all(
            &movies(),
            Some(h.alice),
            by(SortKey::LastPlayed, false),
            ItemFilter::default()
        )
        .await,
        ["1", "5", "3", "2", "7", "8", "9", "4", "6"]
    );
    // And together with the catalog's parts.
    let unwatched_sf = ItemFilter {
        watched: vec![Unwatched],
        genres: vec!["Science Fiction".into()],
        ..ItemFilter::default()
    };
    assert_eq!(
        h.all(&movies(), Some(h.alice), title, unwatched_sf).await,
        ["2"]
    );
}

#[tokio::test]
async fn a_show_is_watched_when_every_cached_episode_is() {
    let h = harness("fed-browse-shows").await;
    // Andor: all three. Bluey: one of two. Chernobyl: none.
    for (n, at) in [(0, 100), (1, 200), (2, 300)] {
        h.saw(
            h.alice,
            &format!("plex:episode:1{n}"),
            Some("plex:show:1"),
            State::Watched,
            at,
        )
        .await;
    }
    h.saw(
        h.alice,
        "plex:episode:20",
        Some("plex:show:2"),
        State::Watched,
        500,
    )
    .await;
    let watched = |s: WatchedState| ItemFilter {
        watched: vec![s],
        ..ItemFilter::default()
    };
    let title = ItemSort::DEFAULT;
    assert_eq!(
        h.all(
            &shows(),
            Some(h.alice),
            title,
            watched(WatchedState::Watched)
        )
        .await,
        ["1"]
    );
    assert_eq!(
        h.all(
            &shows(),
            Some(h.alice),
            title,
            watched(WatchedState::InProgress)
        )
        .await,
        ["2"]
    );
    assert_eq!(
        h.all(
            &shows(),
            Some(h.alice),
            title,
            watched(WatchedState::Unwatched)
        )
        .await,
        ["3"]
    );
    assert_eq!(
        h.all(
            &shows(),
            Some(h.alice),
            by(SortKey::LastPlayed, true),
            ItemFilter::default()
        )
        .await,
        ["2", "1", "3"]
    );
}

#[tokio::test]
async fn filter_values_are_what_the_cached_items_have() {
    let h = harness("fed-browse-values").await;
    let values = h.cache.filter_values(&movies()).await.unwrap();
    let genres: Vec<(&str, Option<u32>)> = values
        .genres
        .iter()
        .map(|g| (g.value.as_str(), g.count))
        .collect();
    assert_eq!(
        genres,
        [
            ("Action", Some(2)),
            ("Animation", Some(1)),
            ("Comedy", Some(2)),
            ("Crime", Some(1)),
            ("Drama", Some(4)),
            ("Horror", Some(1)),
            ("Science Fiction", Some(4)),
        ]
    );
    assert_eq!(
        values.years.first().map(|y| (y.value, y.count)),
        Some((2016, Some(1)))
    );
    assert_eq!(values.years.len(), 9);
    let ratings: Vec<(&str, Option<u32>)> = values
        .content_ratings
        .iter()
        .map(|g| (g.value.as_str(), g.count))
        .collect();
    assert_eq!(
        ratings,
        [("PG", Some(2)), ("PG-13", Some(2)), ("R", Some(5))]
    );
    let resolutions: Vec<(Resolution, Option<u32>)> = values
        .resolutions
        .iter()
        .map(|r| (r.value, r.count))
        .collect();
    assert_eq!(
        resolutions,
        [
            (Resolution::Uhd, Some(3)),
            (Resolution::FullHd, Some(4)),
            (Resolution::Hd, Some(1)),
            (Resolution::Sd, Some(1)),
        ]
    );
    let ranges: Vec<(DynamicRange, Option<u32>)> = values
        .dynamic_ranges
        .iter()
        .map(|r| (r.value, r.count))
        .collect();
    assert_eq!(
        ranges,
        [
            (DynamicRange::Sdr, Some(5)),
            (DynamicRange::Hdr, Some(3)),
            (DynamicRange::DolbyVision, Some(2)),
        ]
    );
    assert_eq!(values.sorts, SortKey::ALL.to_vec());
    assert!(values.filters.contains(&FilterPart::Watched));
    // A TV library: no quality, no ranges.
    let tv = h.cache.filter_values(&shows()).await.unwrap();
    assert!(tv.resolutions.is_empty() && tv.dynamic_ranges.is_empty());
    assert!(!tv.sorts.contains(&SortKey::Size));
}

#[tokio::test]
async fn a_library_not_cached_is_not_found() {
    let h = harness("fed-browse-gone").await;
    let gone = Library {
        id: LibraryId::federated(ORIGIN, &"plex:9".parse().unwrap()).unwrap(),
        ..movies()
    };
    let query = ItemQuery {
        sort: by(SortKey::Added, true),
        filter: ItemFilter::default(),
        viewer: None,
    };
    assert!(matches!(
        h.cache
            .query_items(
                &gone.id,
                &query,
                Page {
                    offset: 0,
                    limit: 10
                }
            )
            .await,
        Err(CatalogError::NotFound(_))
    ));
}
