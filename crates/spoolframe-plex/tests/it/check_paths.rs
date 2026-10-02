//! `PlexCatalog::check_paths` against a fake PMS: one library maps to
//! files that exist, one has no mapping, one has a wrong mapping.

use std::path::{Path, PathBuf};

use spoolframe_catalog::CatalogError;
use spoolframe_plex::{LibraryPaths, PathMap, PathMapping, PlexCatalog, Scope};
use wiremock::matchers::{header, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn json(name: &str) -> ResponseTemplate {
    let file = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    let body = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{file}: {e}"));
    ResponseTemplate::new(200).set_body_raw(body, "application/json")
}

/// A fake PMS with the recorded Movies, TV Shows and Music libraries,
/// each listing two playable items.
async fn pms() -> MockServer {
    let server = MockServer::start().await;
    for (at, leaf_type, fixture) in [
        ("/library/sections/2/all", "1", "section-all-movie"),
        ("/library/sections/1/all", "4", "section-leaves-episode"),
        ("/library/sections/12/all", "10", "section-leaves-track"),
    ] {
        Mock::given(path(at))
            .and(query_param("type", leaf_type))
            .respond_with(json(fixture))
            .mount(&server)
            .await;
    }
    Mock::given(path("/library/sections"))
        .respond_with(json("sections"))
        .mount(&server)
        .await;
    server
}

/// A local tree holding the two recorded movies, and nothing else. One
/// per test: tests run in parallel, and each rebuilds its own tree.
fn movies_root(test: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("check-paths")
        .join(test);
    let _ = std::fs::remove_dir_all(&root);
    for movie in [
        "6.Below.Miracle.on.the.Mountain.2017/6 Below Miracle on the Mountain 2017 720p WEB-DL MkvCage.mkv",
        "24 Days (2014)/24 DaysH2641080pAC3.mkv",
    ] {
        let file = root.join(movie);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"").unwrap();
    }
    root
}

/// Movies map to files that exist; TV Shows have no mapping; Music has
/// one that points nowhere.
fn catalog(server: &MockServer, test: &str) -> PlexCatalog {
    PlexCatalog::new(server.uri().parse().unwrap(), None)
        .unwrap()
        .with_paths(PathMap::new(vec![
            PathMapping {
                library: None,
                from: "/data/Movies".into(),
                to: movies_root(test),
            },
            PathMapping {
                library: Some("Music".into()),
                from: "/data".into(),
                to: "/nonexistent/music".into(),
            },
        ]))
}

fn report<'r>(reports: &'r [LibraryPaths], library: &str) -> &'r LibraryPaths {
    reports
        .iter()
        .find(|r| r.library == library)
        .unwrap_or_else(|| panic!("no report for {library}"))
}

#[tokio::test]
async fn each_library_is_reported_with_its_failures() {
    let server = pms().await;
    let reports = catalog(&server, "each_library_is_reported_with_its_failures")
        .check_paths(None, Scope::All)
        .await
        .unwrap();

    let movies = report(&reports, "Movies");
    assert_eq!(movies.ok, 2);
    assert!(movies.is_ok());

    let shows = report(&reports, "TV Shows");
    assert_eq!(shows.ok, 0);
    assert_eq!(shows.no_mapping.count, 2);
    assert!(
        shows.no_mapping.examples[0].starts_with("/data/TVShows/12 Monkeys/"),
        "{:?}",
        shows.no_mapping.examples
    );

    let music = report(&reports, "Music");
    assert_eq!(music.unreadable.count, 2);
    assert!(
        music.unreadable.examples[0].contains("-> /nonexistent/music/Music/The Cranberries/"),
        "{:?}",
        music.unreadable.examples
    );
}

#[tokio::test]
async fn one_library_can_be_checked_alone() {
    let server = pms().await;
    let reports = catalog(&server, "one_library_can_be_checked_alone")
        .check_paths(Some("Movies"), Scope::All)
        .await
        .unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].library, "Movies");
}

#[tokio::test]
async fn an_unknown_library_is_not_found() {
    let server = pms().await;
    let err = catalog(&server, "an_unknown_library_is_not_found")
        .check_paths(Some("Photos"), Scope::All)
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)), "{err}");
}

#[tokio::test]
async fn a_sample_asks_plex_for_only_a_few_items() {
    let server = MockServer::start().await;
    Mock::given(path("/library/sections"))
        .respond_with(json("sections"))
        .mount(&server)
        .await;
    Mock::given(path("/library/sections/2/all"))
        .and(header("X-Plex-Container-Size", "3"))
        .respond_with(json("section-all-movie"))
        .expect(1)
        .mount(&server)
        .await;
    catalog(&server, "a_sample_asks_plex_for_only_a_few_items")
        .check_paths(Some("Movies"), Scope::Sample(3))
        .await
        .unwrap();
}
