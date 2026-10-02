//! `PlexCatalog` against a fake PMS serving the recorded fixtures.

use std::path::PathBuf;

use spoolframe_catalog::{Catalog, CatalogError, Page};
use spoolframe_plex::{PathMap, PathMapping, PlexCatalog};
use spoolframe_types::{Item, LibraryId, LibraryKind, MediaId, MediaKind, Namespace};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn json(name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(fixture(name), "application/json")
}

async fn serve(server: &MockServer, at: &str, name: &str) {
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(json(name))
        .mount(server)
        .await;
}

fn catalog(server: &MockServer) -> PlexCatalog {
    PlexCatalog::new(server.uri().parse().unwrap(), None).unwrap()
}

fn id(kind: MediaKind, key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, kind, key).unwrap()
}

#[tokio::test]
async fn libraries_come_from_the_sections() {
    let server = MockServer::start().await;
    serve(&server, "/library/sections", "sections").await;
    let kinds: Vec<_> = catalog(&server)
        .libraries()
        .await
        .unwrap()
        .into_iter()
        .map(|l| l.kind)
        .collect();
    assert_eq!(
        kinds,
        [LibraryKind::Movies, LibraryKind::Shows, LibraryKind::Music]
    );
}

#[tokio::test]
async fn a_configured_token_is_sent() {
    let server = MockServer::start().await;
    Mock::given(path("/library/sections"))
        .and(header("X-Plex-Token", "s3cr3t"))
        .respond_with(json("sections"))
        .expect(1)
        .mount(&server)
        .await;
    let catalog = PlexCatalog::new(server.uri().parse().unwrap(), Some("s3cr3t".into())).unwrap();
    catalog.libraries().await.unwrap();
}

#[tokio::test]
async fn no_token_header_is_sent_without_a_token() {
    let server = MockServer::start().await;
    serve(&server, "/library/sections", "sections").await;
    catalog(&server).libraries().await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert!(!requests[0].headers.contains_key("x-plex-token"));
}

#[tokio::test]
async fn a_library_is_listed_one_page_at_a_time() {
    let server = MockServer::start().await;
    Mock::given(path("/library/sections/2/all"))
        .and(header("X-Plex-Container-Start", "40"))
        .and(header("X-Plex-Container-Size", "2"))
        .respond_with(json("section-all-movie"))
        .expect(1)
        .mount(&server)
        .await;
    let page = catalog(&server)
        .items(
            &LibraryId::new(Namespace::Plex, "2").unwrap(),
            Page {
                offset: 40,
                limit: 2,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.total, 770);
    assert_eq!(page.items.len(), 2);
}

#[tokio::test]
async fn a_show_comes_with_its_seasons() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/208618", "metadata-show").await;
    serve(
        &server,
        "/library/metadata/208618/children",
        "children-show",
    )
    .await;
    let got = catalog(&server)
        .item(&id(MediaKind::Show, "208618"))
        .await
        .unwrap();
    assert!(matches!(got.item, Item::Show(_)));
    assert!(
        got.children
            .iter()
            .all(|c| c.id().kind() == MediaKind::Season)
    );
    assert_eq!(got.children.len(), 2);
}

#[tokio::test]
async fn a_movie_has_no_children_and_none_are_fetched() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/134883", "metadata-movie").await;
    Mock::given(path("/library/metadata/134883/children"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let got = catalog(&server)
        .item(&id(MediaKind::Movie, "134883"))
        .await
        .unwrap();
    assert!(got.children.is_empty());
}

#[tokio::test]
async fn an_id_of_the_wrong_kind_is_an_error() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/208618", "metadata-show").await;
    let err = catalog(&server)
        .item(&id(MediaKind::Movie, "208618"))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CatalogError::Backend { message, .. } if message.contains("plex:show:208618")),
        "{err}"
    );
}

#[tokio::test]
async fn search_returns_items_of_several_kinds() {
    let server = MockServer::start().await;
    Mock::given(path("/hubs/search"))
        .and(query_param("query", "cure"))
        .respond_with(json("hubs-search"))
        .mount(&server)
        .await;
    let items = catalog(&server).search("cure", 50).await.unwrap();
    assert!(items.iter().any(|i| matches!(i, Item::Artist(_))));
    assert!(items.iter().any(|i| matches!(i, Item::Movie(_))));
}

#[tokio::test]
async fn search_honours_the_overall_limit() {
    let server = MockServer::start().await;
    serve(&server, "/hubs/search", "hubs-search").await;
    let items = catalog(&server).search("cure", 2).await.unwrap();
    assert_eq!(items.len(), 2);
}

/// A scratch media tree for one test, standing in for the local mount.
fn media_root(test: &str) -> PathBuf {
    let root = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

const EPISODE: &str =
    "TVShows/12 Monkeys/Season 1/12.Monkeys.S01E01.Splinter.Bluray-1080p.Remux.mkv";

fn mapping(library: Option<&str>, from: &str, to: &std::path::Path) -> PathMapping {
    PathMapping {
        library: library.map(str::to_owned),
        from: from.into(),
        to: to.to_owned(),
    }
}

#[tokio::test]
async fn an_episode_resolves_to_its_mapped_local_file() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/208620", "metadata-episode").await;
    let root = media_root("mapped-episode");
    let local = root.join(EPISODE);
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    std::fs::write(&local, b"").unwrap();
    let catalog = catalog(&server).with_paths(PathMap::new(vec![mapping(None, "/data", &root)]));
    let path = catalog
        .media_path(&id(MediaKind::Episode, "208620"))
        .await
        .unwrap();
    assert_eq!(path, local);
}

#[tokio::test]
async fn a_library_scoped_mapping_matches_the_plex_library_title() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/208620", "metadata-episode").await;
    let root = media_root("scoped-episode");
    let local = root.join(EPISODE);
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    std::fs::write(&local, b"").unwrap();
    let catalog = catalog(&server).with_paths(PathMap::new(vec![
        mapping(None, "/data", std::path::Path::new("/nowhere")),
        mapping(Some("TV Shows"), "/data", &root),
    ]));
    let path = catalog
        .media_path(&id(MediaKind::Episode, "208620"))
        .await
        .unwrap();
    assert_eq!(path, local);
}

#[tokio::test]
async fn a_missing_file_names_the_plex_path_the_mapping_and_the_mapped_path() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/208620", "metadata-episode").await;
    let root = media_root("missing-episode");
    let catalog = catalog(&server).with_paths(PathMap::new(vec![mapping(None, "/data", &root)]));
    let err = catalog
        .media_path(&id(MediaKind::Episode, "208620"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, CatalogError::MediaFileUnavailable(_)),
        "{err}"
    );
    let text = err.to_string();
    assert!(text.contains(&format!("/data/{EPISODE}")), "{text}");
    assert!(
        text.contains(&format!("/data -> {}", root.display())),
        "{text}"
    );
    assert!(
        text.contains(&root.join(EPISODE).display().to_string()),
        "{text}"
    );
}

#[tokio::test]
async fn an_unmapped_missing_file_says_no_mapping_applied() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/208620", "metadata-episode").await;
    let err = catalog(&server)
        .media_path(&id(MediaKind::Episode, "208620"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("mapping none"), "{err}");
}

#[tokio::test]
async fn a_show_is_not_playable() {
    let server = MockServer::start().await;
    let err = catalog(&server)
        .media_path(&id(MediaKind::Show, "208618"))
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotPlayable(_)), "{err}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_missing_item_is_not_found_by_its_id() {
    let server = MockServer::start().await;
    Mock::given(path("/library/metadata/999"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let err = catalog(&server)
        .item(&id(MediaKind::Movie, "999"))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CatalogError::NotFound(what) if what == "plex:movie:999"),
        "{err}"
    );
}

#[tokio::test]
async fn a_non_numeric_key_never_reaches_the_pms() {
    let server = MockServer::start().await;
    let err = catalog(&server)
        .item(&id(MediaKind::Movie, "1/../../sections"))
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)), "{err}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_rejected_token_says_so() {
    let server = MockServer::start().await;
    Mock::given(path("/library/sections"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let catalog = PlexCatalog::new(server.uri().parse().unwrap(), Some("old".into())).unwrap();
    let err = catalog.libraries().await.unwrap_err();
    assert!(err.to_string().contains("rejected the token"), "{err}");
}

#[tokio::test]
async fn a_pms_wanting_a_token_says_none_is_configured() {
    let server = MockServer::start().await;
    Mock::given(path("/library/sections"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let err = catalog(&server).libraries().await.unwrap_err();
    assert!(err.to_string().contains("none is configured"), "{err}");
}

#[tokio::test]
async fn a_garbled_response_is_an_error_not_an_empty_list() {
    let server = MockServer::start().await;
    Mock::given(path("/library/sections"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>proxy error</html>"))
        .mount(&server)
        .await;
    let err = catalog(&server).libraries().await.unwrap_err();
    assert!(matches!(err, CatalogError::Backend { .. }), "{err}");
}

#[tokio::test]
async fn an_unreachable_pms_is_named_in_the_error() {
    // A port that was just free: nothing is listening on it.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}");
    let catalog = PlexCatalog::new(url.parse().unwrap(), None).unwrap();
    let err = catalog.libraries().await.unwrap_err();
    assert!(matches!(err, CatalogError::Unreachable { .. }), "{err}");
    assert!(err.to_string().contains(&url), "{err}");
}
