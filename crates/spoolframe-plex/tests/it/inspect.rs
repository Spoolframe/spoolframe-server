//! An admin's look at a Plex library (P3-16) against a fake PMS: rows a
//! page at a time with their files looked for, the quick filters, an
//! item's files and match, and the library check behind "Missing files".
//! The shapes are the ones PMS 1.43 sends with `includeGuids=1`.

use serde_json::{Value, json};
use spoolframe_catalog::{Browse, Catalog, Inspect, Page, RowFilter};
use spoolframe_plex::{PathMap, PathMapping, PlexCatalog, Scope};
use spoolframe_types::{Item, LibraryId, MediaId, MediaKind, Namespace};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn container(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "MediaContainer": body }))
}

fn movie(key: &str, title: &str, guid: &str, file: &str) -> Value {
    json!({
        "ratingKey": key,
        "type": "movie",
        "title": title,
        "year": 2017,
        "guid": guid,
        "thumb": format!("/library/metadata/{key}/thumb/1"),
        "art": format!("/library/metadata/{key}/art/1"),
        "addedAt": 1_507_951_743,
        "updatedAt": 1_786_645_049,
        "Guid": if guid.starts_with("local://") { json!([]) } else { json!([{ "id": "imdb://tt5503688" }, { "id": "tmdb://423087" }]) },
        "Media": [{
            "id": 311_416,
            "container": "mkv",
            "videoCodec": "h264",
            "videoResolution": "720",
            "audioCodec": "aac",
            "audioChannels": 2,
            "bitrate": 1075,
            "duration": 5_863_942,
            "Part": [{ "id": 445_739, "file": file, "size": 787_842_211, "duration": 5_863_942 }],
        }],
    })
}

fn episode(key: &str, index: u32, file: &str) -> Value {
    json!({
        "ratingKey": key,
        "type": "episode",
        "title": format!("Episode {index}"),
        "guid": format!("plex://episode/{key}"),
        "grandparentRatingKey": "100",
        "grandparentTitle": "12 Monkeys",
        "parentRatingKey": "101",
        "parentTitle": "Season 1",
        "parentIndex": 1,
        "index": index,
        "thumb": format!("/library/metadata/{key}/thumb/1"),
        "Media": [{ "id": 1, "Part": [{ "id": 2, "file": file, "size": 1000 }] }],
    })
}

/// Three movies: one whose file is here, one whose file is not, one
/// Plex did not match, with no artwork.
fn movies() -> Vec<Value> {
    let mut unmatched = movie(
        "3",
        "KonoSuba OVA",
        "local://144791",
        "/data/Movies/OVA.mkv",
    );
    unmatched["thumb"] = Value::Null;
    unmatched["art"] = Value::Null;
    vec![
        movie("1", "6 Below", "plex://movie/a", "/data/Movies/Below.mkv"),
        movie("2", "24 Days", "plex://movie/b", "/data/Movies/Days.mkv"),
        unmatched,
    ]
}

async fn pms() -> MockServer {
    let pms = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/library/sections"))
        .respond_with(container(json!({ "Directory": [
            { "key": "2", "type": "movie", "title": "Movies" },
            { "key": "1", "type": "show", "title": "TV Shows" },
        ] })))
        .mount(&pms)
        .await;
    Mock::given(method("GET"))
        .and(path("/identity"))
        .respond_with(container(json!({ "machineIdentifier": "94ad03ca" })))
        .mount(&pms)
        .await;
    pms
}

/// Section 2 lists `movies` a page at a time, with their guids.
async fn serve_movies(pms: &MockServer) {
    let all = movies();
    for (start, size) in [(0usize, 2usize), (2, 2), (0, 50), (0, 0)] {
        let page: Vec<Value> = all.iter().skip(start).take(size).cloned().collect();
        Mock::given(path("/library/sections/2/all"))
            .and(query_param("type", "1"))
            .and(query_param("includeGuids", "1"))
            .and(header("X-Plex-Container-Start", start.to_string().as_str()))
            .and(header("X-Plex-Container-Size", size.to_string().as_str()))
            .respond_with(container(json!({
                "totalSize": all.len(),
                "librarySectionTitle": "Movies",
                "Metadata": page,
            })))
            .mount(pms)
            .await;
    }
    // The whole listing, for the filters Plex cannot apply and the check.
    Mock::given(path("/library/sections/2/all"))
        .and(query_param("type", "1"))
        .respond_with(container(json!({
            "totalSize": all.len(),
            "librarySectionTitle": "Movies",
            "Metadata": all,
        })))
        .with_priority(10)
        .mount(pms)
        .await;
}

fn library(key: &str) -> LibraryId {
    LibraryId::new(Namespace::Plex, key).unwrap()
}

fn id(kind: MediaKind, key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, kind, key).unwrap()
}

/// A catalog mapping `/data/Movies` to a directory holding only
/// `Below.mkv`, kept for the test.
async fn catalog(pms: &MockServer, test: &str) -> (PlexCatalog, spoolframe_test_dir::TestDir) {
    let dir = spoolframe_test_dir::TestDir::new(&format!("spoolframe-inspect-{test}")).await;
    std::fs::write(dir.as_ref().join("Below.mkv"), b"x").unwrap();
    let catalog = PlexCatalog::new(pms.uri().parse().unwrap(), None)
        .unwrap()
        .with_paths(PathMap::new(vec![PathMapping {
            library: None,
            from: "/data/Movies".into(),
            to: dir.to_path_buf(),
        }]));
    (catalog, dir)
}

fn browse(kinds: &[MediaKind], filter: Option<RowFilter>, offset: u32, limit: u32) -> Browse {
    Browse {
        kinds: kinds.to_vec(),
        text: None,
        filter,
        page: Page { offset, limit },
    }
}

fn titles(rows: &[spoolframe_catalog::Row]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r.item {
            Item::Movie(m) => m.title.clone(),
            other => other.id().to_string(),
        })
        .collect()
}

#[tokio::test]
async fn a_page_of_rows_has_its_files_looked_for() {
    let pms = pms().await;
    serve_movies(&pms).await;
    let (plex, dir) = catalog(&pms, "page").await;
    let page = plex
        .browse(&library("2"), &browse(&[MediaKind::Movie], None, 0, 2))
        .await
        .unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(titles(&page.rows), ["6 Below", "24 Days"]);
    let found = &page.rows[0].files[0];
    assert!(found.found(), "{found:?}");
    assert_eq!(found.local, dir.as_ref().join("Below.mkv"));
    assert_eq!(
        found.mapping.as_ref().unwrap().from,
        std::path::Path::new("/data/Movies")
    );
    let missing = &page.rows[1].files[0];
    assert!(!missing.found());
    assert_eq!(
        missing.reported,
        std::path::Path::new("/data/Movies/Days.mkv")
    );
    assert_eq!(page.rows[0].agents, ["imdb://tt5503688", "tmdb://423087"]);
    assert!(page.rows[0].matched());
    assert_eq!(page.rows[0].updated_at, Some(1_786_645_049));
}

#[tokio::test]
async fn the_next_page_starts_where_the_last_ended() {
    let pms = pms().await;
    serve_movies(&pms).await;
    let (plex, _dir) = catalog(&pms, "next-page").await;
    let page = plex
        .browse(&library("2"), &browse(&[MediaKind::Movie], None, 2, 2))
        .await
        .unwrap();
    assert_eq!(titles(&page.rows), ["KonoSuba OVA"]);
}

#[tokio::test]
async fn unmatched_lists_only_what_plex_did_not_match() {
    let pms = pms().await;
    serve_movies(&pms).await;
    let (plex, _dir) = catalog(&pms, "unmatched").await;
    let page = plex
        .browse(
            &library("2"),
            &browse(&[MediaKind::Movie], Some(RowFilter::Unmatched), 0, 50),
        )
        .await
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(titles(&page.rows), ["KonoSuba OVA"]);
    assert!(!page.rows[0].matched());
}

#[tokio::test]
async fn no_artwork_lists_what_clients_show_a_placeholder_for() {
    let pms = pms().await;
    serve_movies(&pms).await;
    let (plex, _dir) = catalog(&pms, "no-artwork").await;
    let page = plex
        .browse(
            &library("2"),
            &browse(&[MediaKind::Movie], Some(RowFilter::NoArtwork), 0, 50),
        )
        .await
        .unwrap();
    assert_eq!(titles(&page.rows), ["KonoSuba OVA"]);
}

#[tokio::test]
async fn a_search_pages_across_kinds_in_turn() {
    let pms = pms().await;
    // "monk" finds 1 show and 3 episodes; the page 1..3 is the last show
    // row and the first two episodes.
    Mock::given(path("/library/sections/1/search"))
        .and(query_param("type", "2"))
        .and(query_param("query", "monk"))
        .and(header("X-Plex-Container-Start", "1"))
        .respond_with(container(json!({ "totalSize": 1, "Metadata": [] })))
        .mount(&pms)
        .await;
    Mock::given(path("/library/sections/1/search"))
        .and(query_param("type", "4"))
        .and(query_param("query", "monk"))
        .and(header("X-Plex-Container-Start", "0"))
        .and(header("X-Plex-Container-Size", "2"))
        .respond_with(container(json!({
            "totalSize": 3,
            "librarySectionTitle": "TV Shows",
            "Metadata": [episode("201", 1, "/tv/a.mkv"), episode("202", 2, "/tv/b.mkv")],
        })))
        .mount(&pms)
        .await;
    let (plex, _dir) = catalog(&pms, "search").await;
    let page = plex
        .browse(
            &library("1"),
            &Browse {
                kinds: vec![MediaKind::Show, MediaKind::Episode],
                text: Some("monk".into()),
                filter: None,
                page: Page {
                    offset: 1,
                    limit: 2,
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(page.total, 4);
    let ids: Vec<String> = page.rows.iter().map(|r| r.item.id().to_string()).collect();
    assert_eq!(ids, ["plex:episode:201", "plex:episode:202"]);
}

/// The check behind "Missing files" lists every file `check-paths`
/// counts as failing, each with its item, and nothing else.
#[tokio::test]
async fn a_library_check_lists_exactly_the_failures_check_paths_counts() {
    let pms = pms().await;
    serve_movies(&pms).await;
    let (plex, _dir) = catalog(&pms, "check").await;
    let check = plex.check_library(&library("2")).await.unwrap();
    let reports = plex.check_paths(Some("Movies"), Scope::All).await.unwrap();
    let failing = reports[0].no_mapping.count + reports[0].unreadable.count;
    assert_eq!(check.ok, reports[0].ok);
    assert_eq!(check.missing.len() as u64, failing);
    let missing: Vec<String> = check
        .missing
        .iter()
        .map(|m| m.row.item.id().to_string())
        .collect();
    assert_eq!(missing, ["plex:movie:2", "plex:movie:3"]);
    assert_eq!(check.missing_under(&id(MediaKind::Movie, "2")), 1);
}

#[tokio::test]
async fn an_item_is_what_clients_get_with_its_files_and_match() {
    let pms = pms().await;
    let mut m = movie("2", "24 Days", "plex://movie/b", "/data/Movies/Days.mkv");
    m["Marker"] = json!([{ "type": "intro", "startTimeOffset": 1000, "endTimeOffset": 50_000 }]);
    Mock::given(path("/library/metadata/2"))
        .respond_with(container(json!({
            "librarySectionID": 2,
            "librarySectionTitle": "Movies",
            "Metadata": [m],
        })))
        .mount(&pms)
        .await;
    let (plex, dir) = catalog(&pms, "item").await;
    let movie = id(MediaKind::Movie, "2");
    let seen = plex.inspect(&movie).await.unwrap();
    assert_eq!(seen.found, plex.item(&movie).await.unwrap());
    assert_eq!(seen.library, Some(library("2")));
    assert_eq!(seen.guid.as_deref(), Some("plex://movie/b"));
    assert!(
        seen.found
            .external_ids
            .contains(&"tmdb://423087".to_owned())
    );
    let version = &seen.versions[0];
    assert_eq!(
        (
            version.id,
            version.video_codec.as_deref(),
            version.audio_channels
        ),
        (Some(311_416), Some("h264"), Some(2))
    );
    let part = &version.parts[0];
    assert_eq!((part.id, part.size), (Some(445_739), Some(787_842_211)));
    assert_eq!(part.file.local, dir.as_ref().join("Days.mkv"));
    assert!(!part.file.found());
    assert_eq!(seen.found.markers.len(), 1);
}

#[tokio::test]
async fn an_episode_names_its_show_and_season() {
    let pms = pms().await;
    Mock::given(path("/library/metadata/201"))
        .respond_with(container(json!({
            "librarySectionID": 1,
            "librarySectionTitle": "TV Shows",
            "Metadata": [episode("201", 1, "/tv/a.mkv")],
        })))
        .mount(&pms)
        .await;
    let (plex, _dir) = catalog(&pms, "episode").await;
    let seen = plex.inspect(&id(MediaKind::Episode, "201")).await.unwrap();
    let names: Vec<(String, String)> = seen
        .ancestors
        .iter()
        .map(|(id, title)| (id.to_string(), title.clone()))
        .collect();
    assert_eq!(
        names,
        [
            ("plex:show:100".to_owned(), "12 Monkeys".to_owned()),
            ("plex:season:101".to_owned(), "Season 1".to_owned()),
        ]
    );
    assert!(seen.versions[0].parts[0].file.mapping.is_none());
}

#[tokio::test]
async fn a_show_s_children_are_rows_with_their_episode_counts() {
    let pms = pms().await;
    Mock::given(path("/library/metadata/100"))
        .respond_with(container(json!({
            "librarySectionID": 1,
            "librarySectionTitle": "TV Shows",
            "Metadata": [{
                "ratingKey": "100", "type": "show", "title": "12 Monkeys",
                "guid": "plex://show/x", "Location": [{ "path": "/slow/media//TVShows/12 Monkeys" }],
            }],
        })))
        .mount(&pms)
        .await;
    Mock::given(path("/library/metadata/100/children"))
        .respond_with(container(json!({
            "librarySectionTitle": "TV Shows",
            "Metadata": [{
                "ratingKey": "101", "type": "season", "title": "Season 1", "index": 1,
                "parentRatingKey": "100", "parentTitle": "12 Monkeys", "leafCount": 13,
            }],
            "Directory": [{ "key": "/library/metadata/100/allLeaves", "title": "All episodes" }],
        })))
        .mount(&pms)
        .await;
    let (plex, _dir) = catalog(&pms, "show").await;
    let seen = plex.inspect(&id(MediaKind::Show, "100")).await.unwrap();
    assert_eq!(seen.children.len(), 1);
    assert_eq!(seen.children[0].leaves, Some(13));
    assert_eq!(
        seen.folders,
        [std::path::PathBuf::from("/slow/media//TVShows/12 Monkeys")]
    );
    assert_eq!(seen.found.children.len(), 1);
}

#[tokio::test]
async fn an_item_opens_in_the_plex_web_app_on_its_server() {
    let pms = pms().await;
    let (plex, _dir) = catalog(&pms, "link").await;
    let link = plex
        .backend_link(&id(MediaKind::Show, "100"))
        .await
        .unwrap();
    assert_eq!(
        link.as_deref(),
        Some(
            "https://app.plex.tv/desktop/#!/server/94ad03ca/details?key=%2Flibrary%2Fmetadata%2F100"
        )
    );
}
