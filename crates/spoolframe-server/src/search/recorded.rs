//! [`search`] over a Plex server replaying replies recorded from a real
//! PMS (1.43.3; the recordings are `spoolframe-plex`'s
//! `tests/fixtures/search`). Sections: Movies (2), TV Shows (1), Music
//! (12). The catalog is the one a server builds, a [`LiveCatalog`] of one
//! Plex server; `tests/it/search.rs` keeps the request's wiring.

use std::sync::Arc;

use serde::Deserialize;
use spoolframe_plex::PlexCatalog;
use spoolframe_protocol::{SearchRequest, SearchResults, TopResult};
use spoolframe_types::{ArtworkKind, Item, Person, SearchKind, SearchScope};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{search, this_year};
use crate::catalog::{Composite, LiveCatalog};

#[derive(Deserialize)]
struct Recording {
    query: String,
    replies: Vec<Recorded>,
}

#[derive(Deserialize)]
struct Recorded {
    path: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    year: Option<String>,
    body: serde_json::Value,
}

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/../spoolframe-plex/tests/fixtures/search/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// A PMS answering these recorded searches, and only as they were asked.
async fn pms(recordings: &[&str]) -> MockServer {
    let pms = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/library/sections"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(fixture("sections"), "application/json"),
        )
        .mount(&pms)
        .await;
    for name in recordings {
        let recording: Recording = serde_json::from_str(&fixture(name)).unwrap();
        for reply in recording.replies {
            let mock = Mock::given(method("GET"))
                .and(path(reply.path.as_str()))
                .and(query_param("query", recording.query.as_str()));
            let mock = match reply.kind {
                Some(kind) => mock.and(query_param("type", kind)),
                None => mock,
            };
            let mock = match reply.year {
                Some(year) => mock.and(query_param("year", year)),
                None => mock.and(query_param_is_missing("year")),
            };
            mock.respond_with(ResponseTemplate::new(200).set_body_json(reply.body))
                .mount(&pms)
                .await;
        }
    }
    pms
}

/// The catalog a server with `pms` as its one Plex server searches, every
/// section served.
struct Searcher {
    catalog: LiveCatalog,
    _pms: MockServer,
}

impl Searcher {
    async fn start(recordings: &[&str]) -> Self {
        let pms = pms(recordings).await;
        let plex = PlexCatalog::new(pms.uri().parse().unwrap(), None).unwrap();
        let catalog = LiveCatalog::new(Some(Arc::new(Composite::single("fulcrum-ng", plex))));
        Self { catalog, _pms: pms }
    }

    async fn search(&self, request: SearchRequest) -> SearchResults {
        search(&self.catalog, &request, this_year()).await.unwrap()
    }
}

fn request(query: &str, scope: SearchScope) -> SearchRequest {
    SearchRequest {
        query: query.into(),
        limit: 10,
        scope,
        kind: None,
        library: None,
    }
}

fn title(item: &Item) -> &str {
    super::title(item)
}

/// Every item in the results: the top match, then each group's.
fn every_item(results: &SearchResults) -> Vec<&Item> {
    let top = match &results.top {
        Some(TopResult::Item(item)) => Some(item),
        _ => None,
    };
    top.into_iter()
        .chain(results.groups.iter().flat_map(|g| &g.items))
        .collect()
}

fn group(results: &SearchResults, kind: SearchKind) -> Option<&spoolframe_protocol::SearchGroup> {
    results.groups.iter().find(|g| g.kind == kind)
}

/// A track that is the top match says whose it is (PX-07), so a client
/// shows more than its title on a blank tile. `low-tide` is hand-written
/// in the shape the PMS sends, not recorded.
#[tokio::test]
async fn a_track_at_the_top_says_whose_it_is_and_has_its_cover() {
    let searcher = Searcher::start(&["low-tide"]).await;
    let results = searcher
        .search(request("low tide", SearchScope::Music))
        .await;
    let Some(TopResult::Item(Item::Track(track))) = &results.top else {
        panic!("{:?}", results.top)
    };
    assert_eq!(
        (
            track.title.as_str(),
            track.artist_name.as_deref(),
            track.album_title.as_deref()
        ),
        ("Low Tide", Some("Mara Quell"), Some("Harbour Lights"))
    );
    let poster: Vec<_> = track
        .artwork
        .iter()
        .filter(|a| a.kind == ArtworkKind::Poster)
        .map(|a| a.id.to_string())
        .collect();
    assert_eq!(poster, ["plex:/library/metadata/900100/thumb/1700000100"]);
}

#[tokio::test]
async fn a_library_returns_only_its_items() {
    let searcher = Searcher::start(&["dune"]).await;
    let everywhere = searcher.search(request("dune", SearchScope::All)).await;
    assert!(group(&everywhere, SearchKind::Movies).is_some());
    let in_shows = searcher
        .search(SearchRequest {
            library: Some("plex:1".parse().unwrap()),
            ..request("dune", SearchScope::All)
        })
        .await;
    let titles: Vec<_> = every_item(&in_shows).into_iter().map(title).collect();
    assert_eq!(titles, ["Dune: Prophecy"]);
}

#[tokio::test]
async fn a_show_is_the_top_match_and_its_episodes_are_left_out_of_all() {
    let searcher = Searcher::start(&["letterkenny"]).await;
    let results = searcher
        .search(request("letterkenny", SearchScope::All))
        .await;
    assert!(
        matches!(&results.top, Some(TopResult::Item(Item::Show(s))) if s.title == "Letterkenny")
    );
    let episodes = group(&results, SearchKind::Episodes).unwrap();
    assert!(episodes.items.is_empty(), "{:?}", episodes.items);
    assert_eq!(episodes.hidden, 3);
    // In their own scope they are all there.
    let results = searcher
        .search(request("letterkenny", SearchScope::Episodes))
        .await;
    assert_eq!(every_item(&results).len(), 3);
}

#[tokio::test]
async fn a_show_beats_its_own_episode_of_the_same_name() {
    let searcher = Searcher::start(&["the-sopranos"]).await;
    let results = searcher
        .search(request("the sopranos", SearchScope::All))
        .await;
    assert!(
        matches!(&results.top, Some(TopResult::Item(Item::Show(s))) if s.title == "The Sopranos")
    );
    assert_eq!(group(&results, SearchKind::Episodes).unwrap().hidden, 1);
}

#[tokio::test]
async fn a_year_at_the_end_finds_that_year_s_film_and_offers_the_others() {
    let searcher = Searcher::start(&["dune-2021", "dune"]).await;
    let results = searcher
        .search(request("dune 2021", SearchScope::All))
        .await;
    assert_eq!(results.year, Some(2021));
    let titles: Vec<_> = every_item(&results).into_iter().map(title).collect();
    assert_eq!(titles, ["Dune: Part One"]);
    let also: Vec<_> = results.without_year.iter().map(title).collect();
    assert_eq!(also, ["Dune: Prophecy", "Dune: Part Two"]);
}

#[tokio::test]
async fn more_says_a_group_has_more_and_see_all_returns_them() {
    let searcher = Searcher::start(&["pilot"]).await;
    let results = searcher
        .search(SearchRequest {
            limit: 5,
            ..request("pilot", SearchScope::Episodes)
        })
        .await;
    let episodes = group(&results, SearchKind::Episodes).unwrap();
    assert_eq!(episodes.items.len(), 5);
    assert!(episodes.more);
    assert_eq!(results.counts.episodes, Some(14));

    let all = searcher
        .search(SearchRequest {
            limit: 50,
            kind: Some(SearchKind::Episodes),
            ..request("pilot", SearchScope::Episodes)
        })
        .await;
    assert_eq!(all.top, None);
    assert_eq!(all.groups.len(), 1);
    assert_eq!(all.groups[0].items.len(), 14);
    assert!(!all.groups[0].more);
}

/// The person at the top of `results`.
fn top_person(results: &SearchResults) -> &Person {
    match &results.top {
        Some(TopResult::Person(person)) => person,
        other => panic!("not a person: {other:?}"),
    }
}

#[tokio::test]
async fn people_are_found_by_name() {
    let searcher = Searcher::start(&["keanu"]).await;
    let results = searcher.search(request("keanu", SearchScope::All)).await;
    // No title has "keanu" in it, so the first person is the top match,
    // and the others are in their group.
    let keanu = top_person(&results);
    assert_eq!(keanu.name, "Keanu Reeves");
    assert_eq!(keanu.count, Some(9));
    let people = &group(&results, SearchKind::People).unwrap().people;
    assert_eq!(people[0].name, "Keanu Karim");
}

#[tokio::test]
async fn every_scope_is_counted() {
    let searcher = Searcher::start(&["dune"]).await;
    let counts = searcher
        .search(request("dune", SearchScope::All))
        .await
        .counts;
    assert_eq!(counts.movies, Some(2));
    assert_eq!(counts.shows, Some(1));
    assert_eq!(counts.episodes, Some(0));
    assert_eq!(counts.music, Some(0));
    // And two actors.
    assert_eq!(counts.all, Some(5));
}

#[tokio::test]
async fn nothing_in_this_scope_counts_the_others() {
    let searcher = Searcher::start(&["letterkenny"]).await;
    let results = searcher
        .search(request("letterkenny", SearchScope::Movies))
        .await;
    assert_eq!(results.top, None);
    assert!(results.groups.is_empty());
    assert_eq!(results.counts.movies, Some(0));
    assert_eq!(results.counts.shows, Some(1));
    assert_eq!(results.counts.episodes, Some(3));
}
