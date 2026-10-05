//! Scoped search (P3-09) against replies recorded from a real PMS
//! (1.43.3): sections Movies (2), TV Shows (1) and Music (12).
//!
//! The fake PMS answers a search only when it is asked with the recorded
//! `type`, `query` and `year`; anything else is a 404 and fails the
//! search. So a test that passes shows the scope went to Plex.

use std::collections::HashSet;

use serde::Deserialize;
use spoolframe_catalog::{CatalogError, Found, FoundGroup, SearchQuery};
use spoolframe_plex::PlexCatalog;
use spoolframe_types::{Item, PersonRole, SearchKind, SearchScope};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Deserialize)]
struct Recording {
    query: String,
    replies: Vec<Reply>,
}

#[derive(Deserialize)]
struct Reply {
    path: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    year: Option<String>,
    body: serde_json::Value,
}

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/search/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// A PMS with the recorded sections and the replies to these searches.
async fn pms(recordings: &[&str]) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/library/sections"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(fixture("sections"), "application/json"),
        )
        .mount(&server)
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
                .mount(&server)
                .await;
        }
    }
    server
}

fn catalog(server: &MockServer) -> PlexCatalog {
    PlexCatalog::new(server.uri().parse().unwrap(), None).unwrap()
}

fn serving(server: &MockServer, sections: &[&str]) -> PlexCatalog {
    catalog(server).serving(Some(sections.iter().map(|s| s.to_string()).collect()))
}

fn query(text: &str, kinds: &[SearchKind]) -> SearchQuery {
    SearchQuery {
        text: text.into(),
        year: None,
        kinds: kinds.to_vec(),
        library: None,
        limit: 10,
    }
}

fn group(found: &Found, kind: SearchKind) -> &FoundGroup {
    found.groups.iter().find(|g| g.kind == kind).unwrap()
}

fn titles(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .map(|i| match i {
            Item::Movie(v) => v.title.clone(),
            Item::Show(v) => v.title.clone(),
            Item::Season(v) => v.title.clone(),
            Item::Episode(v) => v.title.clone(),
            Item::Artist(v) => v.name.clone(),
            Item::Album(v) => v.title.clone(),
            Item::Track(v) => v.title.clone(),
        })
        .collect()
}

/// The `(path, type)` of every search the PMS was asked.
async fn searched(server: &MockServer) -> HashSet<(String, String)> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/search"))
        .map(|r| {
            let kind = r
                .url
                .query_pairs()
                .find(|(k, _)| k == "type")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            (r.url.path().to_owned(), kind)
        })
        .collect()
}

#[tokio::test]
async fn music_asks_the_music_section_for_artists_albums_and_tracks_only() {
    let server = pms(&["cure"]).await;
    let found = catalog(&server)
        .find(&query("cure", SearchScope::Music.kinds()))
        .await
        .unwrap();
    let kinds: Vec<_> = found.groups.iter().map(|g| g.kind).collect();
    assert_eq!(kinds, SearchScope::Music.kinds());
    assert_eq!(
        titles(&group(&found, SearchKind::Artists).items),
        ["The Cure"]
    );
    assert!(
        group(&found, SearchKind::Albums)
            .items
            .iter()
            .all(|i| matches!(i, Item::Album(_)))
    );
    assert!(
        group(&found, SearchKind::Tracks)
            .items
            .iter()
            .all(|i| matches!(i, Item::Track(_)))
    );
    let asked = searched(&server).await;
    let music = |t: &str| ("/library/sections/12/search".to_owned(), t.to_owned());
    assert_eq!(asked, HashSet::from([music("8"), music("9"), music("10")]));
}

#[tokio::test]
async fn episodes_are_asked_for_by_their_own_titles() {
    let server = pms(&["letterkenny"]).await;
    let found = catalog(&server)
        .find(&query("letterkenny", &[SearchKind::Episodes]))
        .await
        .unwrap();
    assert_eq!(
        titles(&group(&found, SearchKind::Episodes).items),
        [
            "Letterkenny Talent Show",
            "The Letterkenny Leave",
            "Letterkenny Spelling Bee"
        ]
    );
}

#[tokio::test]
async fn a_show_found_says_how_many_seasons_and_which_genres_it_has() {
    let server = pms(&["letterkenny"]).await;
    let found = catalog(&server)
        .find(&query("letterkenny", &[SearchKind::Shows]))
        .await
        .unwrap();
    let [Item::Show(show)] = &group(&found, SearchKind::Shows).items[..] else {
        panic!("not one show")
    };
    assert_eq!(show.season_count, Some(6));
    assert_eq!(show.genres, ["Comedy", "Sport"]);
}

#[tokio::test]
async fn a_group_holds_at_most_the_limit_and_says_how_many_there_are() {
    let server = pms(&["pilot"]).await;
    let mut asked = query("pilot", &[SearchKind::Episodes]);
    asked.limit = 5;
    let found = catalog(&server).find(&asked).await.unwrap();
    let episodes = group(&found, SearchKind::Episodes);
    assert_eq!(episodes.items.len(), 5);
    assert_eq!(episodes.total, 14);
}

#[tokio::test]
async fn the_page_size_is_the_limit() {
    let server = pms(&["pilot"]).await;
    let mut asked = query("pilot", &[SearchKind::Episodes]);
    asked.limit = 5;
    catalog(&server).find(&asked).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let search = requests
        .iter()
        .find(|r| r.url.path() == "/library/sections/1/search")
        .unwrap();
    assert_eq!(search.headers["X-Plex-Container-Size"], "5");
}

#[tokio::test]
async fn a_year_goes_to_plex() {
    let server = pms(&["dune-2021"]).await;
    let mut asked = query("dune", &[SearchKind::Movies, SearchKind::Shows]);
    asked.year = Some(2021);
    let found = catalog(&server).find(&asked).await.unwrap();
    assert_eq!(
        titles(&group(&found, SearchKind::Movies).items),
        ["Dune: Part One"]
    );
    assert!(group(&found, SearchKind::Shows).items.is_empty());
}

#[tokio::test]
async fn a_library_is_the_only_section_asked() {
    let server = pms(&["dune"]).await;
    let mut asked = query("dune", &[SearchKind::Movies, SearchKind::Shows]);
    asked.library = Some("plex:1".parse().unwrap());
    let found = catalog(&server).find(&asked).await.unwrap();
    assert_eq!(group(&found, SearchKind::Movies).total, 0);
    assert_eq!(
        titles(&group(&found, SearchKind::Shows).items),
        ["Dune: Prophecy"]
    );
    assert_eq!(
        searched(&server).await,
        HashSet::from([("/library/sections/1/search".to_owned(), "2".to_owned())])
    );
}

#[tokio::test]
async fn a_library_that_is_not_served_is_not_searched() {
    let server = pms(&["dune"]).await;
    let mut asked = query("dune", &[SearchKind::Shows]);
    asked.library = Some("plex:1".parse().unwrap());
    let err = serving(&server, &["2"]).find(&asked).await.unwrap_err();
    assert!(matches!(err, CatalogError::NotServed(_)), "{err}");
    asked.library = Some("plex:99".parse().unwrap());
    let err = catalog(&server).find(&asked).await.unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)), "{err}");
}

#[tokio::test]
async fn sections_that_are_not_served_are_not_searched() {
    let server = pms(&["dune"]).await;
    let found = serving(&server, &["2"])
        .find(&query("dune", &[SearchKind::Movies, SearchKind::Shows]))
        .await
        .unwrap();
    assert_eq!(group(&found, SearchKind::Movies).total, 2);
    assert_eq!(group(&found, SearchKind::Shows).total, 0);
    assert!(
        searched(&server)
            .await
            .iter()
            .all(|(p, _)| p == "/library/sections/2/search")
    );
}

#[tokio::test]
async fn people_come_from_the_actor_hub_once_each() {
    let server = pms(&["keanu"]).await;
    let found = catalog(&server)
        .find(&query("keanu", &[SearchKind::People]))
        .await
        .unwrap();
    let people = &group(&found, SearchKind::People).people;
    let keanu = &people[0];
    assert_eq!(keanu.name, "Keanu Reeves");
    assert_eq!(keanu.id.to_string(), "plex:actor-134469");
    assert_eq!(keanu.role, PersonRole::Actor);
    // 7 films in Movies and 2 in TV Shows, which Plex lists apart.
    assert_eq!(keanu.count, Some(9));
    assert_eq!(
        people.iter().filter(|p| p.name == "Keanu Reeves").count(),
        1
    );
}

#[tokio::test]
async fn people_are_not_looked_for_inside_a_library() {
    let server = pms(&["keanu"]).await;
    let mut asked = query("keanu", &[SearchKind::People]);
    asked.library = Some("plex:2".parse().unwrap());
    let found = catalog(&server).find(&asked).await.unwrap();
    assert!(group(&found, SearchKind::People).people.is_empty());
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().all(|r| r.url.path() != "/hubs/search"));
}

async fn person_pms() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/library/all"))
        .and(query_param("actor", "134469"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("person-actor-134469"), "application/json"),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_person_s_items_are_listed_from_every_section() {
    let server = person_pms().await;
    let items = catalog(&server)
        .person_items(&"plex:actor-134469".parse().unwrap())
        .await
        .unwrap();
    assert_eq!(items.len(), 9);
    assert!(titles(&items).contains(&"Severance".to_owned()));
}

#[tokio::test]
async fn a_person_s_items_in_sections_not_served_are_left_out() {
    let server = person_pms().await;
    let items = serving(&server, &["2"])
        .person_items(&"plex:actor-134469".parse().unwrap())
        .await
        .unwrap();
    assert_eq!(items.len(), 7);
    assert!(items.iter().all(|i| matches!(i, Item::Movie(_))));
}

#[tokio::test]
async fn a_person_id_of_another_shape_is_not_found() {
    let server = person_pms().await;
    for id in [
        "plex:actor-",
        "plex:producer-1",
        "plex:actor-1/../2",
        "plex:134469",
    ] {
        let err = catalog(&server)
            .person_items(&id.parse().unwrap())
            .await
            .unwrap_err();
        assert!(matches!(err, CatalogError::NotFound(_)), "{id}: {err}");
    }
}
