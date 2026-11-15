//! Search with no server, over what a profile holds on this device.

use std::sync::Arc;

use spoolframe_protocol::downloads::{DownloadFile, DownloadSource, DownloadTracks};
use spoolframe_protocol::{SearchRequest, SearchResults, TopResult};
use spoolframe_types::{
    Album, Artist, Item, MediaId, MediaKind, MediaTracks, Namespace, PlaybackState, SearchKind,
    SearchScope, Track,
};

use super::super::store::{EntryKind, Hold, Store};
use super::super::{DownloadPhase, LocalWatch, Shared};
use super::*;
use crate::test_fixtures::movie;

fn id(kind: MediaKind, key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, kind, key).unwrap()
}

fn show(key: &str, title: &str) -> Item {
    serde_json::from_value(serde_json::json!({
        "kind": "show", "id": format!("plex:show:{key}"), "title": title, "year": 2008,
        "summary": null, "content_rating": null, "season_count": 5,
        "genres": [], "artwork": [],
    }))
    .unwrap()
}

fn episode(key: &str, title: &str, show: &str, show_title: &str) -> Item {
    serde_json::from_value(serde_json::json!({
        "kind": "episode", "id": format!("plex:episode:{key}"),
        "show_id": format!("plex:show:{show}"), "show_title": show_title,
        "season_id": "plex:season:1", "season": 1, "episode": 1, "title": title,
        "summary": null, "content_rating": null, "year": null, "air_date": null,
        "duration": 3_000_000, "artwork": [],
        "playback": { "position": null, "watched": false },
    }))
    .unwrap()
}

fn film(key: &str, title: &str, year: u16) -> Item {
    let mut item = movie(&format!("plex:movie:{key}"), title);
    if let Item::Movie(m) = &mut item {
        m.year = Some(year);
    }
    item
}

struct Device {
    shared: Arc<Shared>,
    store: Store,
}

fn device(test: &str) -> Device {
    let dir = std::env::temp_dir().join(format!(
        "sf-sdk-offline-search-{test}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let shared = Shared::new(Some(dir));
    // The client's own store: what is written there is indexed.
    let store = shared.store.clone().unwrap();
    Device { shared, store }
}

impl Device {
    /// `item`, a film or episode, downloaded whole by `user` on "server".
    fn video(&self, user: &str, item: Item, show: Option<Item>) {
        let kind = match item.id().kind() {
            MediaKind::Episode => EntryKind::Episode,
            _ => EntryKind::Film,
        };
        let mut m = Store::manifest(
            kind,
            format!("{user}-{}", item.id()).replace(':', "-"),
            "server".into(),
            user.into(),
            user.into(),
            item,
            MediaTracks::default(),
            DownloadSource::Original,
            DownloadTracks::All,
            LocalWatch::default(),
            1,
        );
        m.show = show;
        m.holds = vec![Hold::download(user)];
        m.phase = DownloadPhase::Done;
        self.store.write(&m).unwrap();
    }

    /// `track`, downloaded whole by "user", its file here.
    fn track(&self, track: Item, album: Option<Item>, artist: Option<Item>) {
        let mut m = Store::manifest(
            EntryKind::Track,
            format!("track-{}", track.id()).replace(':', "-"),
            "server".into(),
            "user".into(),
            "user".into(),
            track,
            MediaTracks::default(),
            DownloadSource::Original,
            DownloadTracks::All,
            LocalWatch::default(),
            1,
        );
        m.album = album;
        m.artist = artist;
        m.holds = vec![Hold::download("user")];
        m.size = Some(3);
        m.offset = 3;
        m.phase = DownloadPhase::Done;
        m.file = Some(DownloadFile {
            container: spoolframe_types::Container::Flac,
            tracks: Default::default(),
        });
        // The file first, as a download finishes: then its manifest.
        let media = self.store.media(&m).unwrap();
        std::fs::create_dir_all(media.parent().unwrap()).unwrap();
        std::fs::write(media, b"abc").unwrap();
        self.store.write(&m).unwrap();
    }

    /// A film's manifest of `user`, written by another store than the
    /// client's (another process, a build before): not indexed as it is
    /// written.
    fn film_elsewhere(&self, user: &str, key: &str, title: &str) {
        let mut m = Store::manifest(
            EntryKind::Film,
            format!("{user}-elsewhere-{key}"),
            "server".into(),
            user.into(),
            user.into(),
            film(key, title, 2000),
            MediaTracks::default(),
            DownloadSource::Original,
            DownloadTracks::All,
            LocalWatch::default(),
            1,
        );
        m.holds = vec![Hold::download(user)];
        m.phase = DownloadPhase::Done;
        Store::new(self.store.root().to_path_buf())
            .write(&m)
            .unwrap();
    }

    fn search(&self, user: &str, request: SearchRequest) -> SearchResults {
        search(&self.shared, "server", user, request).unwrap()
    }
}

fn all(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.into(),
        limit: 10,
        scope: SearchScope::All,
        kind: None,
        library: None,
    }
}

fn top(results: &SearchResults) -> String {
    match &results.top {
        Some(TopResult::Item(item)) => item.id().to_string(),
        other => panic!("no top item: {other:?}"),
    }
}

/// Every item shown, the top one first.
fn shown(results: &SearchResults) -> Vec<String> {
    let top = match &results.top {
        Some(TopResult::Item(item)) => Some(item.id().to_string()),
        _ => None,
    };
    top.into_iter()
        .chain(
            results
                .groups
                .iter()
                .flat_map(|g| g.items.iter().map(|i| i.id().to_string())),
        )
        .collect()
}

fn music(device: &Device) {
    let artist = Item::Artist(Artist {
        merge: Default::default(),
        id: id(MediaKind::Artist, "30"),
        name: "Sigur Rós".into(),
        summary: None,
        artwork: Vec::new(),
    });
    let album = Item::Album(Album {
        merge: Default::default(),
        id: id(MediaKind::Album, "31"),
        artist_id: id(MediaKind::Artist, "30"),
        artist_name: Some("Sigur Rós".into()),
        title: "Ágætis byrjun".into(),
        year: Some(1999),
        track_count: Some(10),
        artwork: Vec::new(),
    });
    let track = Item::Track(Track {
        merge: Default::default(),
        id: id(MediaKind::Track, "32"),
        album_id: id(MediaKind::Album, "31"),
        artist_id: id(MediaKind::Artist, "30"),
        artist_name: Some("Sigur Rós".into()),
        album_title: Some("Ágætis byrjun".into()),
        title: "Svefn-g-englar".into(),
        track: Some(2),
        disc: Some(1),
        album_track_count: None,
        codec: None,
        bit_depth: None,
        sample_rate: None,
        duration: None,
        artwork: Vec::new(),
        playback: PlaybackState::default(),
    });
    device.track(track, Some(album), Some(artist));
}

#[test]
fn accents_and_case_are_folded_both_ways() {
    let device = device("accents");
    device.video("user", film("1", "Amélie", 2001), None);
    device.video("user", film("2", "Die Straße", 1923), None);
    assert_eq!(top(&device.search("user", all("AMELIE"))), "plex:movie:1");
    assert_eq!(top(&device.search("user", all("strasse"))), "plex:movie:2");
    assert_eq!(top(&device.search("user", all("amélie"))), "plex:movie:1");
}

#[test]
fn each_word_of_the_query_starts_a_word_of_the_title() {
    let device = device("prefix");
    device.video("user", film("1", "Star Wars: A New Hope", 1977), None);
    device.video("user", film("2", "Lone Star", 1996), None);
    assert_eq!(
        shown(&device.search("user", all("wars sta"))),
        ["plex:movie:1"]
    );
    // Inside a word is not the start of one.
    assert!(shown(&device.search("user", all("ars"))).is_empty());
}

#[test]
fn a_leading_article_counts_neither_in_the_query_nor_the_title() {
    let device = device("article");
    device.video("user", film("1", "The Thing", 1982), None);
    device.video("user", film("2", "Thing of Beauty", 2009), None);
    assert_eq!(top(&device.search("user", all("thing"))), "plex:movie:1");
    assert_eq!(
        top(&device.search("user", all("the thing"))),
        "plex:movie:1"
    );
}

#[test]
fn an_episode_is_found_by_its_show_s_title_and_its_show_with_it() {
    let device = device("episode-show");
    device.video(
        "user",
        episode("9", "Pilot", "5", "Breaking Bad"),
        Some(show("5", "Breaking Bad")),
    );
    let results = device.search("user", all("breaking"));
    assert_eq!(top(&results), "plex:show:5");
    let episodes = results
        .groups
        .iter()
        .find(|g| g.kind == SearchKind::Episodes)
        .unwrap();
    // Listed in All, its show being the top match and in no shows row;
    // and found under the Episodes scope.
    let listed: Vec<String> = episodes.items.iter().map(|i| i.id().to_string()).collect();
    assert_eq!(listed, ["plex:episode:9"]);
    assert_eq!(episodes.hidden, 0);
    assert!(results.groups.iter().all(|g| !g.items.is_empty()));
    let scoped = device.search(
        "user",
        SearchRequest {
            scope: SearchScope::Episodes,
            ..all("breaking pil")
        },
    );
    assert_eq!(shown(&scoped), ["plex:episode:9"]);
}

#[test]
fn music_is_found_by_track_album_and_artist() {
    let device = device("music");
    music(&device);
    let results = device.search("user", all("sigur"));
    assert_eq!(top(&results), "plex:artist:30");
    assert_eq!(
        shown(&results),
        ["plex:artist:30", "plex:album:31", "plex:track:32"]
    );
    assert_eq!(results.counts.music, Some(3));
    assert_eq!(top(&device.search("user", all("agaetis"))), "plex:album:31");
}

#[test]
fn a_scope_and_a_kind_search_only_their_own() {
    let device = device("scope");
    device.video("user", film("1", "Heima", 2007), None);
    music(&device);
    let movies = device.search(
        "user",
        SearchRequest {
            scope: SearchScope::Movies,
            ..all("heima")
        },
    );
    assert_eq!(shown(&movies), ["plex:movie:1"]);
    let music_only = device.search(
        "user",
        SearchRequest {
            scope: SearchScope::Music,
            ..all("sigur")
        },
    );
    assert!(shown(&music_only).iter().all(|i| !i.contains("movie")));
    let see_all = device.search(
        "user",
        SearchRequest {
            kind: Some(SearchKind::Tracks),
            ..all("sigur")
        },
    );
    assert_eq!(see_all.top, None);
    assert_eq!(see_all.groups.len(), 1);
    assert_eq!(shown(&see_all), ["plex:track:32"]);
}

#[test]
fn each_group_is_cut_to_the_limit_and_says_there_is_more() {
    let device = device("limit");
    for n in 1..=5 {
        device.video(
            "user",
            film(&n.to_string(), &format!("Alien {n}"), 2000),
            None,
        );
    }
    let results = device.search(
        "user",
        SearchRequest {
            limit: 2,
            kind: Some(SearchKind::Movies),
            ..all("alien")
        },
    );
    let movies = &results.groups[0];
    assert_eq!(movies.items.len(), 2);
    assert!(movies.more);
    assert_eq!(movies.total, Some(5));
}

#[test]
fn an_exact_title_is_the_top_match_over_one_that_starts_with_it() {
    let device = device("exact");
    device.video("user", film("1", "Aliens", 1986), None);
    device.video("user", film("2", "Alien: Romulus", 2024), None);
    device.video("user", film("3", "Alien", 1979), None);
    assert_eq!(top(&device.search("user", all("alien"))), "plex:movie:3");
}

#[test]
fn a_profile_does_not_find_another_s_downloads() {
    let device = device("profiles");
    device.video("bob", film("1", "Alien", 1979), None);
    device.video("user", film("2", "Aliens", 1986), None);
    assert_eq!(
        shown(&device.search("user", all("alien"))),
        ["plex:movie:2"]
    );
}

#[test]
fn a_locked_profile_finds_nothing() {
    let device = device("locked");
    device.video("user", film("1", "Alien", 1979), None);
    device.shared.locks.seen("server", "user", true);
    assert!(matches!(
        search(&device.shared, "server", "user", all("alien")),
        Err(SdkError::ProfileLocked)
    ));
}

#[test]
fn an_empty_query_finds_nothing() {
    let device = device("empty");
    device.video("user", film("1", "Alien", 1979), None);
    for query in ["", "   ", "?!"] {
        assert_eq!(device.search("user", all(query)), SearchResults::default());
    }
}

#[test]
fn a_year_at_the_end_filters_and_offers_other_years() {
    let device = device("year");
    device.video("user", film("1", "Dune", 2021), None);
    device.video("user", film("2", "Dune", 1984), None);
    let results = device.search("user", all("dune 1984"));
    assert_eq!(results.year, Some(1984));
    assert_eq!(shown(&results), ["plex:movie:2"]);
    let others: Vec<_> = results
        .without_year
        .iter()
        .map(|i| i.id().to_string())
        .collect();
    assert_eq!(others, ["plex:movie:1"]);
}

#[test]
fn with_nothing_in_the_scope_the_other_scopes_are_counted() {
    let device = device("counts");
    device.video("user", film("1", "Alien", 1979), None);
    let results = device.search(
        "user",
        SearchRequest {
            scope: SearchScope::Shows,
            ..all("alien")
        },
    );
    assert!(results.groups.is_empty());
    assert_eq!(results.counts.movies, Some(1));
}

/// A database made where downloads are already (the build before, or a
/// new profile) indexes them from their manifests.
#[test]
fn the_index_is_built_from_the_manifests_when_it_is_missing() {
    let device = device("rebuild");
    device.film_elsewhere("user", "1", "Alien");
    assert_eq!(top(&device.search("user", all("alien"))), "plex:movie:1");
}

/// A download deleted is no longer found, and its show goes with its
/// last episode.
#[test]
fn a_deleted_download_leaves_the_search() {
    let device = device("deleted");
    device.video("user", film("1", "Alien", 1979), None);
    device.video(
        "user",
        episode("9", "Pilot", "5", "Breaking Bad"),
        Some(show("5", "Breaking Bad")),
    );
    let before = (
        shown(&device.search("user", all("alien"))),
        shown(&device.search("user", all("breaking"))).len(),
    );
    for m in device.store.scan() {
        device.store.remove(&m).unwrap();
    }
    assert_eq!(
        (
            before,
            shown(&device.search("user", all("alien"))),
            shown(&device.search("user", all("breaking"))),
        ),
        (
            // The show on top, its episode listed.
            (vec!["plex:movie:1".to_owned()], 2),
            Vec::<String>::new(),
            Vec::<String>::new()
        )
    );
}

/// The exact title is the top match though more candidates match than the
/// limit lets through: they are not cut alphabetically before ranking.
#[test]
fn an_exact_title_past_the_limit_is_still_the_top_match() {
    let device = device("exact-past-limit");
    device.video("user", film("1", "Arc Begins", 2001), None);
    device.video("user", film("2", "Batman Begins", 2005), None);
    device.video("user", film("3", "Begins", 2010), None);
    let results = device.search(
        "user",
        SearchRequest {
            limit: 1,
            ..all("begins")
        },
    );
    let movies = results
        .groups
        .iter()
        .find(|g| g.kind == SearchKind::Movies)
        .unwrap();
    assert_eq!(
        (top(&results), movies.items.len(), movies.more, movies.total),
        ("plex:movie:3".to_owned(), 1, true, Some(3))
    );
}
