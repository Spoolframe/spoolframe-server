//! Scoped, grouped search (P3-09) through the client protocol, from a
//! Plex server replaying replies recorded from a real PMS (1.43.3; the
//! recordings are `spoolframe-plex`'s `tests/fixtures/search`). Sections:
//! Movies (2), TV Shows (1), Music (12). What a search finds is tested on
//! `search::search` itself, in `search::recorded`; these check that a
//! request reaches it, and a person's items are paged.

use std::sync::Arc;

use serde::Deserialize;
use spoolframe_plex::PlexCatalog;
use spoolframe_protocol::{
    Reply, Request, Response, SearchRequest, SearchResults, SessionId, TopResult,
};
use spoolframe_server::catalog::{Composite, LiveCatalog};
use spoolframe_types::{Item, Person, SearchKind, SearchScope};
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
    let top = match &results.top {
        Some(TopResult::Item(item)) => Some(item),
        _ => None,
    };
    top.into_iter()
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

/// The person at the top of `results`.
fn top_person(results: &SearchResults) -> &Person {
    match &results.top {
        Some(TopResult::Person(person)) => person,
        other => panic!("not a person: {other:?}"),
    }
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
    let keanu = top_person(&results).id.clone();
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
