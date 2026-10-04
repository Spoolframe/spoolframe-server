//! Scoped, grouped search (P3-09) through the client protocol, from a
//! Plex server replaying replies recorded from a real PMS (1.43.3; the
//! recordings are `spoolframe-plex`'s `tests/fixtures/search`). Sections:
//! Movies (2), TV Shows (1), Music (12).

use std::sync::Arc;

use serde::Deserialize;
use spoolframe_plex::PlexCatalog;
use spoolframe_protocol::{Reply, Request, Response, SearchRequest, SearchResults, SessionId};
use spoolframe_server::catalog::{Composite, LiveCatalog};
use spoolframe_types::{Item, SearchKind, SearchScope};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{self, ServerOptions, TestClient, TestServer};

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

/// A server whose one Plex server is `pms`, every section served, and a
/// client signed in to it.
struct Searcher {
    server: TestServer,
    client: TestClient,
    session: SessionId,
}

impl Searcher {
    async fn start(test: &str, pms: &MockServer) -> Self {
        let plex = PlexCatalog::new(pms.uri().parse().unwrap(), None).unwrap();
        let live = LiveCatalog::new(Some(Arc::new(Composite::single("fulcrum-ng", plex))));
        let options = ServerOptions {
            catalog: Some(Arc::new(live)),
            ..ServerOptions::default()
        };
        let server = TestServer::start_with(support::fresh_dir(test).await, options).await;
        server.add_user("alice", "correct horse", None).await;
        let client = server.client().await;
        client.hello().await;
        let session = client.login("alice", "correct horse").await;
        Self {
            server,
            client,
            session,
        }
    }

    async fn search(&self, request: SearchRequest) -> SearchResults {
        match self
            .client
            .call(Some(&self.session), Request::Search(request))
            .await
        {
            Response::Ok(Reply::Search(results)) => results,
            other => panic!("{other:?}"),
        }
    }

    async fn stop(self) {
        self.client.close().await;
        self.server.stop().await;
    }
}

fn request(query: &str, scope: SearchScope) -> SearchRequest {
    SearchRequest {
        scope,
        ..support::search(query)
    }
}

fn title(item: &Item) -> &str {
    match item {
        Item::Movie(v) => &v.title,
        Item::Show(v) => &v.title,
        Item::Season(v) => &v.title,
        Item::Episode(v) => &v.title,
        Item::Artist(v) => &v.name,
        Item::Album(v) => &v.title,
        Item::Track(v) => &v.title,
    }
}

/// Every item in the results: the top match, then each group's.
fn every_item(results: &SearchResults) -> Vec<&Item> {
    results
        .top
        .iter()
        .chain(results.groups.iter().flat_map(|g| &g.items))
        .collect()
}

fn kind_of(item: &Item) -> SearchKind {
    match item {
        Item::Movie(_) => SearchKind::Movies,
        Item::Show(_) => SearchKind::Shows,
        Item::Episode(_) => SearchKind::Episodes,
        Item::Artist(_) => SearchKind::Artists,
        Item::Album(_) => SearchKind::Albums,
        Item::Track(_) => SearchKind::Tracks,
        Item::Season(_) => panic!("a season is never a search result"),
    }
}

fn group(results: &SearchResults, kind: SearchKind) -> Option<&spoolframe_protocol::SearchGroup> {
    results.groups.iter().find(|g| g.kind == kind)
}

#[tokio::test]
async fn each_scope_returns_only_its_kinds() {
    let pms = pms(&["dune", "letterkenny", "cure"]).await;
    let searcher = Searcher::start("search-scopes", &pms).await;
    for (query, scope) in [
        ("dune", SearchScope::Movies),
        ("dune", SearchScope::Shows),
        ("letterkenny", SearchScope::Episodes),
        ("cure", SearchScope::Music),
    ] {
        let results = searcher.search(request(query, scope)).await;
        let items = every_item(&results);
        assert!(!items.is_empty(), "{query} in {scope:?} found nothing");
        for item in items {
            assert!(
                scope.kinds().contains(&kind_of(item)),
                "{query} in {scope:?}: {}",
                title(item)
            );
        }
        for g in &results.groups {
            assert!(scope.kinds().contains(&g.kind), "{query} in {scope:?}");
        }
    }
    searcher.stop().await;
}

#[tokio::test]
async fn a_library_returns_only_its_items() {
    let pms = pms(&["dune"]).await;
    let searcher = Searcher::start("search-library", &pms).await;
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
    searcher.stop().await;
}

#[tokio::test]
async fn a_show_is_the_top_match_and_its_episodes_are_left_out_of_all() {
    let pms = pms(&["letterkenny"]).await;
    let searcher = Searcher::start("search-quiet", &pms).await;
    let results = searcher
        .search(request("letterkenny", SearchScope::All))
        .await;
    assert!(matches!(&results.top, Some(Item::Show(s)) if s.title == "Letterkenny"));
    let episodes = group(&results, SearchKind::Episodes).unwrap();
    assert!(episodes.items.is_empty(), "{:?}", episodes.items);
    assert_eq!(episodes.hidden, 3);
    // In their own scope they are all there.
    let results = searcher
        .search(request("letterkenny", SearchScope::Episodes))
        .await;
    assert_eq!(every_item(&results).len(), 3);
    searcher.stop().await;
}

#[tokio::test]
async fn a_show_beats_its_own_episode_of_the_same_name() {
    let pms = pms(&["the-sopranos"]).await;
    let searcher = Searcher::start("search-same-name", &pms).await;
    let results = searcher
        .search(request("the sopranos", SearchScope::All))
        .await;
    assert!(matches!(&results.top, Some(Item::Show(s)) if s.title == "The Sopranos"));
    assert_eq!(group(&results, SearchKind::Episodes).unwrap().hidden, 1);
    searcher.stop().await;
}

#[tokio::test]
async fn a_year_at_the_end_finds_that_year_s_film_and_offers_the_others() {
    let pms = pms(&["dune-2021", "dune"]).await;
    let searcher = Searcher::start("search-year", &pms).await;
    let results = searcher
        .search(request("dune 2021", SearchScope::All))
        .await;
    assert_eq!(results.year, Some(2021));
    let titles: Vec<_> = every_item(&results).into_iter().map(title).collect();
    assert_eq!(titles, ["Dune: Part One"]);
    let also: Vec<_> = results.without_year.iter().map(title).collect();
    assert_eq!(also, ["Dune: Prophecy", "Dune: Part Two"]);
    searcher.stop().await;
}

#[tokio::test]
async fn more_says_a_group_has_more_and_see_all_returns_them() {
    let pms = pms(&["pilot"]).await;
    let searcher = Searcher::start("search-more", &pms).await;
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
    searcher.stop().await;
}

#[tokio::test]
async fn people_are_found_by_name() {
    let pms = pms(&["keanu"]).await;
    let searcher = Searcher::start("search-people", &pms).await;
    let results = searcher.search(request("keanu", SearchScope::All)).await;
    let people = &group(&results, SearchKind::People).unwrap().people;
    assert_eq!(people[0].name, "Keanu Reeves");
    assert_eq!(people[0].count, Some(9));
    searcher.stop().await;
}

#[tokio::test]
async fn choosing_a_person_lists_their_items_a_page_at_a_time() {
    let pms = pms(&["keanu"]).await;
    Mock::given(method("GET"))
        .and(path("/library/all"))
        .and(query_param("actor", "134469"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("person-actor-134469"), "application/json"),
        )
        .mount(&pms)
        .await;
    let searcher = Searcher::start("search-person", &pms).await;
    let results = searcher.search(request("keanu", SearchScope::All)).await;
    let keanu = group(&results, SearchKind::People).unwrap().people[0]
        .id
        .clone();
    let page = |offset| {
        let person = keanu.clone();
        let searcher = &searcher;
        async move {
            let request = Request::PersonItems {
                person,
                offset,
                limit: 5,
            };
            match searcher.client.call(Some(&searcher.session), request).await {
                Response::Ok(Reply::Items { items, total }) => (items, total),
                other => panic!("{other:?}"),
            }
        }
    };
    let (first, total) = page(0).await;
    assert_eq!((first.len(), total), (5, 9));
    let (rest, _) = page(5).await;
    assert_eq!(rest.len(), 4);
    let titles: Vec<_> = first.iter().chain(&rest).map(title).collect();
    assert!(titles.contains(&"John Wick: Chapter 4"), "{titles:?}");
    assert!(titles.contains(&"Severance"), "{titles:?}");
    searcher.stop().await;
}

#[tokio::test]
async fn every_scope_is_counted() {
    let pms = pms(&["dune"]).await;
    let searcher = Searcher::start("search-counts", &pms).await;
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
    searcher.stop().await;
}

#[tokio::test]
async fn nothing_in_this_scope_counts_the_others() {
    let pms = pms(&["letterkenny"]).await;
    let searcher = Searcher::start("search-empty", &pms).await;
    let results = searcher
        .search(request("letterkenny", SearchScope::Movies))
        .await;
    assert_eq!(results.top, None);
    assert!(results.groups.is_empty());
    assert_eq!(results.counts.movies, Some(0));
    assert_eq!(results.counts.shows, Some(1));
    assert_eq!(results.counts.episodes, Some(3));
    searcher.stop().await;
}
