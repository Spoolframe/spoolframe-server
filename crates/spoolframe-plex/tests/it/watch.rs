//! The owner's watch state through `PlexCatalog` (PX-04), against a fake
//! PMS serving listings recorded from the household one.

use spoolframe_plex::PlexCatalog;
use spoolframe_types::{MediaId, MediaKind, Namespace};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn json(name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(fixture(name), "application/json")
}

fn id(kind: MediaKind, key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, kind, key).unwrap()
}

/// A PMS with the recorded sections and the four watch listings: section
/// 2 holds movies, section 1 shows.
async fn pms() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path("/library/sections"))
        .respond_with(json("sections"))
        .mount(&server)
        .await;
    for (section, plex_type, filter, value, name) in [
        ("1", "4", "unwatched", "0", "watch-episodes-watched"),
        ("1", "4", "inProgress", "1", "watch-episodes-in-progress"),
        ("2", "1", "unwatched", "0", "watch-movies-watched"),
        ("2", "1", "inProgress", "1", "watch-movies-in-progress"),
    ] {
        Mock::given(path(format!("/library/sections/{section}/all")))
            .and(query_param("type", plex_type))
            .and(query_param(filter, value))
            .and(query_param("includeGuids", "1"))
            .respond_with(json(name))
            .mount(&server)
            .await;
    }
    server
}

#[tokio::test]
async fn watched_and_in_progress_items_are_read_with_their_ids() {
    let server = pms().await;
    let catalog = PlexCatalog::new(server.uri().parse().unwrap(), None).unwrap();
    let states = catalog.watch_states().await.unwrap();
    assert_eq!(states.len(), 8);

    let watched = states
        .iter()
        .find(|w| w.id == id(MediaKind::Episode, "208620"))
        .unwrap();
    assert!(watched.watched);
    assert_eq!(watched.position_ms, None);
    assert_eq!(watched.show, Some(id(MediaKind::Show, "208618")));
    assert_eq!(watched.last_viewed_at, Some(1_720_402_357));
    assert!(
        watched
            .external_ids
            .iter()
            .any(|g| g.starts_with("imdb://"))
    );

    // 28 Days Later, 36 minutes in.
    let started = states
        .iter()
        .find(|w| w.id == id(MediaKind::Movie, "219079"))
        .unwrap();
    assert!(!started.watched);
    assert_eq!(started.position_ms, Some(2_186_392));
    assert_eq!(started.show, None);
}

#[tokio::test]
async fn writes_go_to_plex_s_watch_endpoints() {
    let server = MockServer::start().await;
    for (at, extra) in [
        ("/:/scrobble", None),
        ("/:/unscrobble", None),
        ("/:/progress", Some(("time", "61000"))),
    ] {
        let mut mock = Mock::given(method("GET"))
            .and(path(at))
            .and(query_param("identifier", "com.plexapp.plugins.library"))
            .and(query_param("key", "229060"));
        if let Some((name, value)) = extra {
            mock = mock
                .and(query_param(name, value))
                .and(query_param("state", "stopped"));
        }
        mock.respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
    }
    let catalog = PlexCatalog::new(server.uri().parse().unwrap(), None).unwrap();
    let banshee = id(MediaKind::Episode, "229060");
    catalog.set_watched(&banshee, true).await.unwrap();
    catalog.set_watched(&banshee, false).await.unwrap();
    catalog.set_position(&banshee, 61_000).await.unwrap();
}

#[tokio::test]
async fn a_key_plex_cannot_have_is_refused_before_any_call() {
    let server = MockServer::start().await;
    let catalog = PlexCatalog::new(server.uri().parse().unwrap(), None).unwrap();
    assert!(
        catalog
            .set_watched(&id(MediaKind::Movie, "../library"), true)
            .await
            .is_err()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}
