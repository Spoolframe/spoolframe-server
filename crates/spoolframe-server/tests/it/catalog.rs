//! Catalog browsing for admins (P3-16) through the admin API, against a
//! stand-in Plex server with a Movies and a TV Shows library whose files
//! are partly on this host: a library's rows with their mapped paths and
//! files, "Missing files" against the path check (P1-04), an item as a
//! client gets it, and consumers kept out.

use std::collections::BTreeSet;
use std::time::Duration;

use jsonrpsee::ws_client::WsClient;
use serde_json::{Value, json};
use spoolframe_admin_api::{
    AdminApiClient, CatalogFilter, CatalogKind, CatalogPage, CatalogQuery, MeApiClient, NewUser,
    PathsChecked, Role, error_code,
};
use spoolframe_catalog::Catalog;
use spoolframe_plex::{PathMap, PathMapping, PlexCatalog};
use wiremock::{MockServer, Request, Respond, ResponseTemplate};

use crate::admin::{anonymous, code, creds, with_token};
use crate::settings::served;

/// The stand-in PMS: every item, answered by path, `type`, `query` and
/// the paging headers as PMS 1.43 answers them.
struct Pms {
    items: Vec<Value>,
}

fn section_of(item: &Value) -> &str {
    item["librarySectionID"].as_str().unwrap_or("")
}

fn header(req: &Request, name: &str) -> Option<usize> {
    req.headers.get(name)?.to_str().ok()?.parse().ok()
}

fn container(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "MediaContainer": body }))
}

fn title_of(section: &str) -> &'static str {
    if section == "2" { "Movies" } else { "TV Shows" }
}

impl Respond for Pms {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let path = req.url.path();
        let query: Vec<(String, String)> = req.url.query_pairs().into_owned().collect();
        let param = |name: &str| {
            query
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        match parts.as_slice() {
            ["identity"] => container(json!({ "machineIdentifier": "94ad03ca" })),
            ["library", "sections"] => container(json!({ "Directory": [
                { "key": "2", "type": "movie", "title": "Movies" },
                { "key": "1", "type": "show", "title": "TV Shows" },
            ] })),
            ["library", "sections", section, what @ ("all" | "search")] => {
                let kind = match param("type").as_deref() {
                    Some("1") => "movie",
                    Some("2") => "show",
                    Some("3") => "season",
                    Some("4") => "episode",
                    _ => "none",
                };
                let text = param("query").map(|q| q.to_lowercase());
                let all: Vec<Value> = self
                    .items
                    .iter()
                    .filter(|i| section_of(i) == *section && i["type"] == kind)
                    .filter(|i| {
                        *what == "all"
                            || text.as_ref().is_some_and(|t| {
                                i["title"].as_str().unwrap().to_lowercase().contains(t)
                            })
                    })
                    .cloned()
                    .collect();
                let start = header(req, "X-Plex-Container-Start").unwrap_or(0);
                let size = header(req, "X-Plex-Container-Size").unwrap_or(usize::MAX);
                let page: Vec<Value> = all.iter().skip(start).take(size).cloned().collect();
                container(json!({
                    "totalSize": all.len(),
                    "librarySectionTitle": title_of(section),
                    "Metadata": page,
                }))
            }
            ["library", "metadata", key, "children"] => {
                let children: Vec<Value> = self
                    .items
                    .iter()
                    .filter(|i| i["parentRatingKey"] == *key)
                    .cloned()
                    .collect();
                let section = children.first().map(section_of).unwrap_or("1");
                container(json!({
                    "librarySectionTitle": title_of(section),
                    "Metadata": children,
                }))
            }
            ["library", "metadata", key] => {
                match self.items.iter().find(|i| i["ratingKey"] == *key) {
                    Some(item) => container(json!({
                        "librarySectionID": section_of(item),
                        "librarySectionTitle": title_of(section_of(item)),
                        "Metadata": [item],
                    })),
                    None => ResponseTemplate::new(404),
                }
            }
            _ => ResponseTemplate::new(404),
        }
    }
}

fn media(file: &str) -> Value {
    json!([{
        "id": 311_416, "container": "mkv", "videoCodec": "hevc", "videoResolution": "4k",
        "audioCodec": "eac3", "audioChannels": 6, "bitrate": 18_000, "duration": 3_004_512,
        "Part": [{ "id": 445_739, "file": file, "size": 4_100_000_000u64, "duration": 3_004_512 }],
    }])
}

fn movie(key: &str, title: &str, guid: &str) -> Value {
    json!({
        "ratingKey": key, "type": "movie", "title": title, "year": 2017,
        "librarySectionID": "2", "guid": guid,
        "thumb": format!("/library/metadata/{key}/thumb/1"),
        "art": format!("/library/metadata/{key}/art/1"),
        "Guid": [{ "id": "imdb://tt5503688" }, { "id": "tmdb://423087" }],
        "Media": media(&format!("/data/movies/{title}.mkv")),
    })
}

fn episode(key: &str, season: (&str, u32), index: u32, title: &str) -> Value {
    json!({
        "ratingKey": key, "type": "episode", "title": title,
        "librarySectionID": "1", "guid": format!("plex://episode/{key}"),
        "grandparentRatingKey": "100", "grandparentTitle": "Severance",
        "parentRatingKey": season.0, "parentTitle": format!("Season {}", season.1),
        "parentIndex": season.1, "index": index, "duration": 3_004_512,
        "summary": "The innies head out on a team-building retreat in the woods.",
        "thumb": format!("/library/metadata/{key}/thumb/1"),
        "Guid": [{ "id": "tvdb://10292371" }, { "id": "tmdb://5604311" }, { "id": "imdb://tt27560178" }],
        "Media": media(&format!("/data/tv/Severance/S{:02}E{index:02}.mkv", season.1)),
        "Marker": [
            { "type": "intro", "startTimeOffset": 42_000, "endTimeOffset": 92_000 },
            { "type": "credits", "startTimeOffset": 2_830_000, "endTimeOffset": 3_004_513, "final": true },
        ],
    })
}

/// Movies: two matched, one of whose files is missing, and one Plex did
/// not match, with no artwork. TV Shows: Severance, two seasons, one
/// episode's file missing; and The Bear, every file there.
fn catalog() -> Vec<Value> {
    let mut unmatched = movie("3", "KonoSuba OVA", "local://144791");
    unmatched["Guid"] = json!([]);
    unmatched["thumb"] = Value::Null;
    unmatched["art"] = Value::Null;
    let show = |key: &str, title: &str, seasons: u32, episodes: u32| {
        json!({
            "ratingKey": key, "type": "show", "title": title, "year": 2022,
            "librarySectionID": "1", "guid": format!("plex://show/{key}"),
            "thumb": format!("/library/metadata/{key}/thumb/1"),
            "childCount": seasons, "leafCount": episodes,
            "Guid": [{ "id": "tvdb://371980" }],
            "Location": [{ "path": format!("/data/tv/{title}") }],
        })
    };
    let season = |key: &str, show: &str, title: &str, n: u32| {
        json!({
            "ratingKey": key, "type": "season", "title": format!("Season {n}"), "index": n,
            "librarySectionID": "1", "parentRatingKey": show, "parentTitle": title,
            "guid": format!("plex://season/{key}"), "leafCount": 2,
        })
    };
    let mut bear = episode("301", ("300", 1), 1, "System");
    bear["grandparentRatingKey"] = json!("200");
    bear["grandparentTitle"] = json!("The Bear");
    bear["Media"] = media("/data/tv/The Bear/S01E01.mkv");
    vec![
        movie("1", "6 Below", "plex://movie/a"),
        movie("2", "24 Days", "plex://movie/b"),
        unmatched,
        show("100", "Severance", 2, 3),
        season("101", "100", "Severance", 1),
        season("102", "100", "Severance", 2),
        episode("111", ("101", 1), 1, "Good News About Hell"),
        episode("121", ("102", 2), 4, "Woe's Hollow"),
        episode("122", ("102", 2), 5, "Trojan's Horse"),
        show("200", "The Bear", 1, 1),
        season("300", "200", "The Bear", 1),
        bear,
    ]
}

/// The files on this host: every one but "24 Days", "KonoSuba OVA" and
/// Trojan's Horse.
fn write_files(root: &std::path::Path) {
    for file in [
        "movies/6 Below.mkv",
        "tv/Severance/S01E01.mkv",
        "tv/Severance/S02E04.mkv",
        "tv/The Bear/S01E01.mkv",
    ] {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }
}

struct Setup {
    pms: MockServer,
    root: WsClient,
    addr: std::net::SocketAddr,
    media: spoolframe_test_dir::TestDir,
    _dir: spoolframe_test_dir::TestDir,
}

async fn setup(test: &str) -> Setup {
    let pms = MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(Pms { items: catalog() })
        .mount(&pms)
        .await;
    let media = spoolframe_test_dir::TestDir::new(&format!("spoolframe-catalog-{test}")).await;
    write_files(media.as_ref());
    let file = format!(
        r#"plex {{ url = "{}"; path_mapping = [ {{ from = "/data"; to = "{}"; }} ]; }}"#,
        pms.uri(),
        media.as_ref().display()
    );
    let (addr, root, dir) = served(&format!("catalog-{test}"), &file).await;
    Setup {
        pms,
        root,
        addr,
        media,
        _dir: dir,
    }
}

fn query(library: &str) -> CatalogQuery {
    CatalogQuery {
        library: library.into(),
        kind: None,
        text: None,
        filter: None,
        offset: 0,
        limit: 50,
    }
}

fn ids(page: &CatalogPage) -> Vec<String> {
    page.rows.iter().map(|r| r.item.id().to_string()).collect()
}

/// Wait for `library`'s path check to be done.
async fn checked(root: &WsClient, library: &str) -> (u64, u64) {
    for _ in 0..200 {
        match root.catalog_library(library.into()).await.unwrap().paths {
            PathsChecked::Checked { ok, missing, .. } => return (ok, missing),
            PathsChecked::Failed { error, .. } => panic!("{error}"),
            _ => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
    panic!("the check of {library} never finished");
}

#[tokio::test]
async fn a_library_page_lists_its_items_with_mapped_paths_and_flags_a_missing_file() {
    let s = setup("movies").await;
    let page = s.root.catalog_rows(query("plex:2")).await.unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(ids(&page), ["plex:movie:1", "plex:movie:2", "plex:movie:3"]);
    let found = &page.rows[0].files[0];
    assert_eq!(found.plex_path, "/data/movies/6 Below.mkv");
    assert_eq!(
        found.mapped_path,
        s.media
            .as_ref()
            .join("movies/6 Below.mkv")
            .display()
            .to_string()
    );
    assert_eq!(found.mapping.as_ref().unwrap().from, "/data");
    assert!(found.found);
    let missing = &page.rows[1].files[0];
    assert!(!missing.found, "{missing:?}");
    assert!(missing.problem.is_some());
    assert_eq!(page.rows[0].agents, ["imdb://tt5503688", "tmdb://423087"]);
    assert!(page.rows[0].matched && !page.rows[2].matched);
}

#[tokio::test]
async fn a_page_is_the_slice_asked_for() {
    let s = setup("paging").await;
    let page = s
        .root
        .catalog_rows(CatalogQuery {
            offset: 1,
            limit: 1,
            ..query("plex:2")
        })
        .await
        .unwrap();
    assert_eq!(
        (page.total, ids(&page)),
        (3, vec!["plex:movie:2".to_owned()])
    );
    let too_many = s
        .root
        .catalog_rows(CatalogQuery {
            limit: 201,
            ..query("plex:2")
        })
        .await
        .unwrap_err();
    assert_eq!(code(too_many), error_code::INVALID_INPUT);
}

#[tokio::test]
async fn the_library_header_counts_each_kind_and_the_quick_filters() {
    let s = setup("header").await;
    let shows = s.root.catalog_library("plex:1".into()).await.unwrap();
    let counts: Vec<(CatalogKind, u32)> = shows.counts.iter().map(|c| (c.kind, c.count)).collect();
    assert_eq!(
        counts,
        [
            (CatalogKind::Show, 2),
            (CatalogKind::Season, 3),
            (CatalogKind::Episode, 4),
        ]
    );
    assert_eq!(shows.plex_title.as_deref(), Some("TV Shows"));
    let movies = s.root.catalog_library("plex:2".into()).await.unwrap();
    assert_eq!((movies.unmatched, movies.no_artwork), (1, 1));
    let unmatched = s
        .root
        .catalog_rows(CatalogQuery {
            filter: Some(CatalogFilter::Unmatched),
            ..query("plex:2")
        })
        .await
        .unwrap();
    assert_eq!(ids(&unmatched), ["plex:movie:3"]);
}

/// "Missing files" is the path check's own list: the same files
/// `plexCheckPaths` (Server › Media paths, `check-paths`) counts as
/// failing, one row per episode, and no other.
#[tokio::test]
async fn filtering_to_missing_files_lists_exactly_what_the_path_check_reports() {
    let s = setup("missing").await;
    let (ok, missing) = checked(&s.root, "plex:1").await;
    let page = s
        .root
        .catalog_rows(CatalogQuery {
            filter: Some(CatalogFilter::Missing),
            ..query("plex:1")
        })
        .await
        .unwrap();
    let listed: BTreeSet<String> = page
        .rows
        .iter()
        .flat_map(|r| r.files.iter().filter(|f| !f.found))
        .map(|f| f.plex_path.clone())
        .collect();

    let reports = s
        .root
        .plex_check_paths(Some("TV Shows".into()))
        .await
        .unwrap()
        .reports;
    let report = &reports[0];
    assert_eq!(ok, report.ok);
    assert_eq!(missing, report.no_mapping.count + report.unreadable.count);
    let reported: BTreeSet<String> = report
        .no_mapping
        .examples
        .iter()
        .chain(&report.unreadable.examples)
        .map(|e| e.split(" -> ").next().unwrap().to_owned())
        .collect();
    assert_eq!(listed, reported);
    assert_eq!(page.total as u64, missing);
    assert_eq!(ids(&page), ["plex:episode:122"]);
}

/// Server › Media paths' check is the one the library pages list, with
/// nothing checked again.
#[tokio::test]
async fn the_media_paths_check_is_the_one_missing_files_lists() {
    let s = setup("media-paths").await;
    s.root.plex_check_paths(None).await.unwrap();
    let page = s
        .root
        .catalog_rows(CatalogQuery {
            filter: Some(CatalogFilter::Missing),
            ..query("plex:2")
        })
        .await
        .unwrap();
    assert!(
        matches!(page.paths, PathsChecked::Checked { missing: 2, .. }),
        "{:?}",
        page.paths
    );
    assert_eq!(ids(&page), ["plex:movie:2", "plex:movie:3"]);
}

#[tokio::test]
async fn a_show_counts_its_missing_episodes_and_opens_into_its_seasons() {
    let s = setup("rollup").await;
    checked(&s.root, "plex:1").await;
    let page = s.root.catalog_rows(query("plex:1")).await.unwrap();
    let missing: Vec<(String, Option<u64>)> = page
        .rows
        .iter()
        .map(|r| (r.item.id().to_string(), r.missing))
        .collect();
    assert_eq!(
        missing,
        [
            ("plex:show:100".to_owned(), Some(1)),
            ("plex:show:200".to_owned(), Some(0)),
        ]
    );
    assert_eq!(page.rows[0].leaves, Some(3));
    let seasons = s
        .root
        .catalog_children("plex:1".into(), "plex:show:100".into())
        .await
        .unwrap();
    let missing: Vec<Option<u64>> = seasons.iter().map(|r| r.missing).collect();
    assert_eq!(missing, [Some(0), Some(1)]);
    let episodes = s
        .root
        .catalog_children("plex:1".into(), "plex:season:102".into())
        .await
        .unwrap();
    let found: Vec<bool> = episodes.iter().map(|r| r.files[0].found).collect();
    assert_eq!(found, [true, false]);
}

#[tokio::test]
async fn a_search_finds_shows_and_episodes_by_title() {
    let s = setup("search").await;
    let page = s
        .root
        .catalog_rows(CatalogQuery {
            text: Some("hollow".into()),
            ..query("plex:1")
        })
        .await
        .unwrap();
    assert_eq!(ids(&page), ["plex:episode:121"]);
    let none = s
        .root
        .catalog_rows(CatalogQuery {
            text: Some("sevrance".into()),
            filter: Some(CatalogFilter::Missing),
            ..query("plex:1")
        })
        .await
        .unwrap();
    // While the check runs, and after: nothing by that name is missing.
    assert!(none.rows.is_empty());
}

/// The item page's "As clients get it" and Show JSON are the client
/// protocol's `Item` reply for it, with its external IDs, markers and
/// artwork.
#[tokio::test]
async fn an_item_page_shows_what_a_client_would_get() {
    let s = setup("item").await;
    let seen = s
        .root
        .catalog_item("plex:episode:121".into())
        .await
        .unwrap();
    let plex = PlexCatalog::new(s.pms.uri().parse().unwrap(), None)
        .unwrap()
        .with_paths(PathMap::new(vec![PathMapping {
            library: None,
            from: "/data".into(),
            to: s.media.to_path_buf(),
        }]));
    let found = plex
        .item(&"plex:episode:121".parse().unwrap())
        .await
        .unwrap();
    let client = spoolframe_protocol::Reply::Item {
        item: found.item,
        children: found.children,
        markers: found.markers,
    };
    assert_eq!(seen.reply, serde_json::to_value(&client).unwrap());
    assert_eq!(seen.reply["markers"].as_array().unwrap().len(), 2);
    assert_eq!(
        seen.reply["item"]["artwork"][0]["id"],
        "plex:/library/metadata/121/thumb/1"
    );
    assert_eq!(
        seen.external_ids,
        [
            "plex://episode/121",
            "tvdb://10292371",
            "tmdb://5604311",
            "imdb://tt27560178"
        ]
    );
    let crumbs: Vec<&str> = seen.ancestors.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(crumbs, ["Severance", "Season 2"]);
    assert_eq!(seen.library.as_ref().unwrap().title, "TV Shows");
    let part = &seen.versions[0].parts[0];
    assert!(part.file.found);
    assert_eq!(part.size, Some(4_100_000_000));
    assert_eq!(seen.versions[0].video_codec.as_deref(), Some("hevc"));
    assert_eq!(
        seen.open_in_plex.as_deref(),
        Some(
            "https://app.plex.tv/desktop/#!/server/94ad03ca/details?key=%2Flibrary%2Fmetadata%2F121"
        )
    );
}

#[tokio::test]
async fn a_consumer_gets_forbidden() {
    let s = setup("consumer").await;
    s.root
        .user_add(NewUser {
            username: "alice".into(),
            display_name: None,
            password: "battery staple".into(),
            role: Role::Consumer,
        })
        .await
        .unwrap();
    let alice = anonymous(s.addr)
        .await
        .login(creds("alice", "battery staple"))
        .await
        .unwrap();
    let alice_api = with_token(s.addr, &alice.token).await;
    let forbidden = |r: Result<(), jsonrpsee::core::ClientError>| {
        assert_eq!(code(r.unwrap_err()), error_code::FORBIDDEN);
    };
    forbidden(alice_api.catalog_library("plex:1".into()).await.map(drop));
    forbidden(alice_api.catalog_rows(query("plex:1")).await.map(drop));
    forbidden(
        alice_api
            .catalog_children("plex:1".into(), "plex:show:100".into())
            .await
            .map(drop),
    );
    forbidden(
        alice_api
            .catalog_item("plex:movie:1".into())
            .await
            .map(drop),
    );
    forbidden(
        alice_api
            .catalog_artwork("plex:movie:1".into())
            .await
            .map(drop),
    );
    forbidden(
        alice_api
            .catalog_check_paths("plex:1".into())
            .await
            .map(drop),
    );
    let image = reqwest::Client::new()
        .get(format!(
            "http://{}/catalog/artwork?id=plex:/library/metadata/1/thumb/1",
            s.addr
        ))
        .bearer_auth(&alice.token)
        .send()
        .await
        .unwrap();
    assert_eq!(image.status(), reqwest::StatusCode::FORBIDDEN);
}

/// A check belongs to the mappings it ran with: changing them makes the
/// library's next look check again.
#[tokio::test]
async fn a_changed_mapping_checks_the_library_again() {
    let s = setup("remapped").await;
    checked(&s.root, "plex:2").await;
    s.root
        .setting_set(
            "plex.path_mapping".into(),
            json!([{ "from": "/data", "to": "/nowhere" }]),
        )
        .await
        .unwrap();
    let (ok, missing) = checked(&s.root, "plex:2").await;
    assert_eq!((ok, missing), (0, 3));
}

/// The server keeps no copy of the catalog: with Plex down, the library
/// page says so rather than showing an old one.
#[tokio::test]
async fn a_library_whose_plex_is_down_is_unavailable() {
    // Nothing answers there.
    let file = r#"plex { url = "http://127.0.0.1:9"; }
        libraries [ { name = "Movies"; kind = "movies"; plex { server = "plex"; section = "2"; } } ]"#;
    let (_addr, root, _dir) = served("catalog-down", file).await;
    let err = root.catalog_rows(query("plex:2")).await.unwrap_err();
    assert_eq!(code(err), error_code::UNAVAILABLE);
    let err = root.catalog_library("plex:2".into()).await.unwrap_err();
    assert_eq!(code(err), error_code::UNAVAILABLE);
}
