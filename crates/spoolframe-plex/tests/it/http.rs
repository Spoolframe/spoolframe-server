//! `PlexCatalog` against a fake PMS serving the recorded fixtures.

use spoolframe_catalog::{Catalog, CatalogError, Page};
use spoolframe_plex::{PathMap, PathMapping, PlexCatalog};
use spoolframe_types::{
    Item, LibraryId, LibraryKind, Marker, MarkerKind, MediaId, MediaKind, Namespace,
};
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

/// The Cure's "Catch" single: three 24-bit, 96 kHz FLAC tracks.
async fn serve_catch(server: &MockServer) {
    serve(server, "/library/metadata/224629", "metadata-album-catch").await;
    serve(
        server,
        "/library/metadata/224629/children",
        "children-album-catch",
    )
    .await;
}

fn tracks(children: &[Item]) -> Vec<&spoolframe_types::Track> {
    children
        .iter()
        .map(|c| match c {
            Item::Track(t) => t,
            other => panic!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn an_album_s_tracks_come_with_their_format_and_the_album_s_count() {
    let server = MockServer::start().await;
    serve_catch(&server).await;
    // Plex gives streams only on items asked for by key, so the tracks
    // are asked for together, once.
    Mock::given(method("GET"))
        .and(path("/library/metadata/224630,224631,224632"))
        .respond_with(json("metadata-tracks-catch"))
        .expect(1)
        .mount(&server)
        .await;
    let got = catalog(&server)
        .item(&id(MediaKind::Album, "224629"))
        .await
        .unwrap();
    let Item::Album(album) = &got.item else {
        panic!("not an album")
    };
    assert_eq!(album.track_count, Some(3));
    let tracks = tracks(&got.children);
    let shown: Vec<_> = tracks
        .iter()
        .map(|t| {
            (
                t.track,
                t.album_track_count,
                t.codec,
                t.bit_depth,
                t.sample_rate,
            )
        })
        .collect();
    let flac = Some(spoolframe_types::AudioCodec::Flac);
    assert_eq!(
        shown,
        [
            (Some(1), Some(3), flac, Some(24), Some(96_000)),
            (Some(2), Some(3), flac, Some(24), Some(96_000)),
            (Some(3), Some(3), flac, Some(24), Some(96_000)),
        ]
    );
}

#[tokio::test]
async fn an_album_s_tracks_load_without_their_format_when_plex_cannot_give_it() {
    let server = MockServer::start().await;
    serve_catch(&server).await;
    Mock::given(path("/library/metadata/224630,224631,224632"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let got = catalog(&server)
        .item(&id(MediaKind::Album, "224629"))
        .await
        .unwrap();
    let tracks = tracks(&got.children);
    assert_eq!(tracks.len(), 3);
    for t in tracks {
        assert_eq!(
            (t.album_track_count, t.bit_depth, t.sample_rate),
            (Some(3), None, None)
        );
        assert_eq!(t.codec, Some(spoolframe_types::AudioCodec::Flac));
    }
}

#[tokio::test]
async fn a_track_alone_has_its_format() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/223610", "metadata-track").await;
    let got = catalog(&server)
        .item(&id(MediaKind::Track, "223610"))
        .await
        .unwrap();
    let Item::Track(track) = got.item else {
        panic!("not a track")
    };
    assert_eq!(
        (track.bit_depth, track.sample_rate),
        (Some(16), Some(44_100))
    );
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
async fn an_episode_comes_with_its_intro_and_credits() {
    let server = MockServer::start().await;
    // Plex sends markers only when asked to.
    Mock::given(path("/library/metadata/208620"))
        .and(query_param("includeMarkers", "1"))
        .respond_with(json("metadata-episode-markers"))
        .mount(&server)
        .await;
    serve(&server, "/library/metadata/208620", "metadata-episode").await;
    let got = catalog(&server)
        .item(&id(MediaKind::Episode, "208620"))
        .await
        .unwrap();
    // In order of start, whatever order Plex lists them in; its
    // `commercial` marker is left out.
    assert_eq!(
        got.markers,
        [
            Marker {
                kind: MarkerKind::Intro,
                start: std::time::Duration::from_millis(258_865),
                end: std::time::Duration::from_millis(319_865),
                to_end: false,
            },
            Marker {
                kind: MarkerKind::Credits,
                start: std::time::Duration::from_millis(2_815_419),
                // 1 ms past the episode's duration, as Plex has it.
                end: std::time::Duration::from_millis(2_852_689),
                to_end: true,
            },
        ]
    );
}

/// The recorded episode, with `markers` for its `Marker` list.
async fn serve_episode_with_markers(server: &MockServer, markers: serde_json::Value) {
    let mut body: serde_json::Value =
        serde_json::from_str(&fixture("metadata-episode-markers")).unwrap();
    body["MediaContainer"]["Metadata"][0]["Marker"] = markers;
    Mock::given(path("/library/metadata/208620"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// Markers are extra: one Plex sends in a shape this build cannot read,
/// of a type it uses or not, is left out, and the item still loads.
#[tokio::test]
async fn a_marker_that_cannot_be_read_is_left_out_and_the_item_loads() {
    let server = MockServer::start().await;
    let markers = serde_json::json!([
        { "type": "commercial", "startTimeOffset": 600_000 },
        { "type": "credits", "startTimeOffset": "soon", "endTimeOffset": 2_852_689, "final": true },
        { "type": "intro", "startTimeOffset": 258_865.5, "endTimeOffset": 319_865 },
        { "startTimeOffset": 1, "endTimeOffset": 2 },
        "not a marker",
        { "type": "intro", "startTimeOffset": 30_000, "endTimeOffset": 90_000 },
    ]);
    serve_episode_with_markers(&server, markers).await;
    let got = catalog(&server)
        .item(&id(MediaKind::Episode, "208620"))
        .await
        .unwrap();
    assert_eq!(
        got.markers,
        [Marker {
            kind: MarkerKind::Intro,
            start: std::time::Duration::from_millis(30_000),
            end: std::time::Duration::from_millis(90_000),
            to_end: false,
        }]
    );
}

#[tokio::test]
async fn a_marker_that_does_not_end_after_it_starts_is_left_out() {
    let server = MockServer::start().await;
    let markers = serde_json::json!([
        { "type": "intro", "startTimeOffset": 90_000, "endTimeOffset": 30_000 },
        { "type": "credits", "startTimeOffset": 2_815_419, "endTimeOffset": 2_815_419, "final": true },
    ]);
    serve_episode_with_markers(&server, markers).await;
    let got = catalog(&server)
        .item(&id(MediaKind::Episode, "208620"))
        .await
        .unwrap();
    assert_eq!(got.markers, []);
}

#[tokio::test]
async fn an_item_plex_has_no_markers_for_has_none() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/208620", "metadata-episode").await;
    let got = catalog(&server)
        .item(&id(MediaKind::Episode, "208620"))
        .await
        .unwrap();
    assert!(got.markers.is_empty());
}

#[tokio::test]
async fn markers_are_asked_for_only_with_a_movie_s_or_episode_s_details() {
    let server = MockServer::start().await;
    Mock::given(query_param("includeMarkers", "1"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    serve(&server, "/library/metadata/208620", "metadata-episode").await;
    serve(&server, "/library/metadata/208619", "metadata-season").await;
    serve(
        &server,
        "/library/metadata/208619/children",
        "children-season",
    )
    .await;
    let tmp = media_root("no-markers-for-a-path").await;
    let local = tmp.as_ref().join(EPISODE);
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    std::fs::write(&local, b"").unwrap();
    let catalog =
        catalog(&server).with_paths(PathMap::new(vec![mapping(None, "/data", tmp.as_ref())]));
    // Neither the file's path nor a season and its episodes need them.
    catalog
        .media_path(&id(MediaKind::Episode, "208620"))
        .await
        .unwrap();
    catalog
        .item(&id(MediaKind::Season, "208619"))
        .await
        .unwrap();
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
async fn external_ids_take_one_request_without_children_or_markers() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/208618", "metadata-show").await;
    let ids = catalog(&server)
        .external_ids(&id(MediaKind::Show, "208618"))
        .await
        .unwrap();
    assert!(
        ids.iter().any(|id| id.starts_with("plex://show/")),
        "{ids:?}"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.query(), None);
}

/// Plex rebuilt its database and the key is a show's now: the movie that
/// had it is not there.
#[tokio::test]
async fn external_ids_of_a_key_plex_gave_to_another_kind_are_not_found() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/208618", "metadata-show").await;
    let err = catalog(&server)
        .external_ids(&id(MediaKind::Movie, "208618"))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, CatalogError::NotFound(what) if what == "plex:movie:208618"),
        "{err}"
    );
}

/// A scratch media tree for one test, standing in for the local mount: a
/// directory of its own under a random name, removed when dropped.
async fn media_root(test: &str) -> spoolframe_test_dir::TestDir {
    spoolframe_test_dir::TestDir::new(&format!("spoolframe-plex-{test}")).await
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
    let tmp = media_root("mapped-episode").await;
    let root = tmp.as_ref();
    let local = root.join(EPISODE);
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    std::fs::write(&local, b"").unwrap();
    let catalog = catalog(&server).with_paths(PathMap::new(vec![mapping(None, "/data", root)]));
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
    let tmp = media_root("scoped-episode").await;
    let root = tmp.as_ref();
    let local = root.join(EPISODE);
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    std::fs::write(&local, b"").unwrap();
    let catalog = catalog(&server).with_paths(PathMap::new(vec![
        mapping(None, "/data", std::path::Path::new("/nowhere")),
        mapping(Some("TV Shows"), "/data", root),
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
    let tmp = media_root("missing-episode").await;
    let root = tmp.as_ref();
    let catalog = catalog(&server).with_paths(PathMap::new(vec![mapping(None, "/data", root)]));
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

fn artwork_id(key: &str) -> spoolframe_types::ArtworkId {
    spoolframe_types::ArtworkId::new(Namespace::Plex, key).unwrap()
}

#[tokio::test]
async fn artwork_is_fetched_with_its_content_type() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/134883", "metadata-movie").await;
    Mock::given(path("/library/metadata/134883/thumb/1786645049"))
        .and(header("accept", "image/*"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(b"\x89PNG fake".to_vec(), "image/png"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let image = catalog(&server)
        .artwork(&artwork_id("/library/metadata/134883/thumb/1786645049"))
        .await
        .unwrap();
    assert_eq!(image.content_type, "image/png");
    assert_eq!(image.bytes, b"\x89PNG fake");
}

#[tokio::test]
async fn an_artwork_key_that_is_not_an_image_path_never_reaches_the_pms() {
    let server = MockServer::start().await;
    for key in ["/library/sections/1/all", "/:/prefs"] {
        let err = catalog(&server)
            .artwork(&artwork_id(key))
            .await
            .unwrap_err();
        assert!(matches!(err, CatalogError::NotFound(_)), "{key}: {err}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_reply_that_is_not_an_image_is_an_error() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/134883", "metadata-movie").await;
    Mock::given(path("/library/metadata/134883/art/2"))
        .respond_with(json("sections"))
        .mount(&server)
        .await;
    let err = catalog(&server)
        .artwork(&artwork_id("/library/metadata/134883/art/2"))
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::Backend { .. }), "{err}");
}

#[tokio::test]
async fn an_item_comes_with_its_external_ids() {
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
    assert!(
        got.external_ids.contains(&"imdb://tt3148266".to_owned()),
        "{:?}",
        got.external_ids
    );
}

#[tokio::test]
async fn recently_added_is_read_newest_first() {
    let server = MockServer::start().await;
    Mock::given(path("/library/recentlyAdded"))
        .and(header("X-Plex-Container-Size", "3"))
        .respond_with(json("recently-added"))
        .expect(1)
        .mount(&server)
        .await;
    let items = catalog(&server).recently_added(3).await.unwrap();
    assert_eq!(items.len(), 3);
    assert!(items.iter().all(|i| i.id().kind() == MediaKind::Season));
}

/// A catalog serving only the sections with these keys (P3-11).
fn serving(server: &MockServer, sections: &[&str]) -> PlexCatalog {
    catalog(server).serving(Some(sections.iter().map(|s| (*s).to_owned()).collect()))
}

#[tokio::test]
async fn only_the_served_sections_are_libraries() {
    let server = MockServer::start().await;
    serve(&server, "/library/sections", "sections").await;
    let keys: Vec<_> = serving(&server, &["2", "12"])
        .libraries()
        .await
        .unwrap()
        .into_iter()
        .map(|l| l.id.to_string())
        .collect();
    assert_eq!(keys, ["plex:2", "plex:12"]);
    // Every one, for whoever chooses what to serve.
    let all = serving(&server, &["2"]).unrestricted().libraries().await;
    assert_eq!(all.unwrap().len(), 3);
}

#[tokio::test]
async fn an_item_in_a_section_not_served_is_not_served() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/134883", "metadata-movie").await;
    let movie = id(MediaKind::Movie, "134883");
    // The fixture's movie is in section 2.
    let hidden = serving(&server, &["1"]);
    for err in [
        hidden.item(&movie).await.unwrap_err(),
        hidden.external_ids(&movie).await.unwrap_err(),
        hidden.media_path(&movie).await.unwrap_err(),
    ] {
        assert!(
            matches!(&err, CatalogError::NotServed(what) if what == "plex:movie:134883"),
            "{err}"
        );
    }
    let listing = hidden
        .items(
            &LibraryId::new(Namespace::Plex, "2").unwrap(),
            Page {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(listing, CatalogError::NotServed(_)), "{listing}");
    serving(&server, &["2"]).item(&movie).await.unwrap();
}

#[tokio::test]
async fn recently_added_asks_for_more_and_keeps_only_what_is_served() {
    let server = MockServer::start().await;
    // Asked for 2 with sections left out: a wider window, filtered.
    Mock::given(path("/library/recentlyAdded"))
        .and(header("X-Plex-Container-Size", "8"))
        .respond_with(json("recently-added"))
        .expect(2)
        .mount(&server)
        .await;
    let shows = serving(&server, &["1"])
        .recently_added_dated(2)
        .await
        .unwrap();
    assert_eq!(shows.len(), 2);
    assert_eq!(shows[0].0, Some(1_790_839_616));
    assert!(
        serving(&server, &["2"])
            .recently_added(2)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn every_section_is_listed_for_choosing_with_its_size() {
    let server = MockServer::start().await;
    serve(&server, "/library/sections", "sections").await;
    Mock::given(path("/library/sections/2/all"))
        .and(header("X-Plex-Container-Size", "0"))
        .respond_with(json("section-all-movie"))
        .mount(&server)
        .await;
    // Even from a catalog that serves none of them.
    let plex = serving(&server, &[]);
    let sections = plex.sections().await.unwrap();
    let keys: Vec<_> = sections.iter().map(|s| (s.key.as_str(), s.kind)).collect();
    assert_eq!(
        keys,
        [
            ("2", Some(LibraryKind::Movies)),
            ("1", Some(LibraryKind::Shows)),
            ("12", Some(LibraryKind::Music)),
        ]
    );
    assert_eq!(plex.section_size("2").await.unwrap(), 770);
}

fn index() -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/index-sd-trimmed.bif",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[tokio::test]
async fn plex_s_own_preview_index_is_read_for_a_part_that_has_one() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/library/metadata/134883",
        "metadata-movie-indexed",
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/library/parts/445739/indexes/sd"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(index(), "application/octet-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let frames = catalog(&server)
        .preview_frames(&id(MediaKind::Movie, "134883"))
        .await
        .unwrap()
        .expect("Plex has an index for the part");
    let times: Vec<_> = frames.frames.iter().map(|f| f.at_ms).collect();
    assert_eq!(times, [0, 2000, 4000]);
}

#[tokio::test]
async fn a_part_without_an_index_has_none_and_none_is_asked_for() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/134883", "metadata-movie").await;
    Mock::given(path("/library/parts/445739/indexes/sd"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    assert_eq!(
        catalog(&server)
            .preview_frames(&id(MediaKind::Movie, "134883"))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn an_index_plex_lists_but_no_longer_has_is_none() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/library/metadata/134883",
        "metadata-movie-indexed",
    )
    .await;
    Mock::given(path("/library/parts/445739/indexes/sd"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    assert_eq!(
        catalog(&server)
            .preview_frames(&id(MediaKind::Movie, "134883"))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn an_index_that_is_not_one_is_an_error() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/library/metadata/134883",
        "metadata-movie-indexed",
    )
    .await;
    Mock::given(path("/library/parts/445739/indexes/sd"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("<html>", "text/html"))
        .mount(&server)
        .await;
    assert!(matches!(
        catalog(&server)
            .preview_frames(&id(MediaKind::Movie, "134883"))
            .await,
        Err(CatalogError::Backend { .. })
    ));
}

#[tokio::test]
async fn artwork_in_an_excluded_library_never_fetches_image_bytes() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/134883", "metadata-movie").await;
    let restricted = catalog(&server).serving(Some(["3".to_owned()].into()));
    let image = artwork_id("/library/metadata/134883/thumb/1786645049");
    assert!(matches!(
        restricted.artwork(&image).await,
        Err(CatalogError::NotServed(_))
    ));
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path() == "/library/metadata/134883")
    );
    let allowed = catalog(&server).serving(Some(["2".to_owned()].into()));
    allowed.authorize_artwork(&image).await.unwrap();
}

/// Every request is counted for the metrics (P3-14), from the catalog and
/// every view made of it; a 404 is an answer, not an error.
#[tokio::test]
async fn requests_and_their_failures_are_counted_across_views() {
    let server = MockServer::start().await;
    serve(&server, "/library/sections", "sections").await;
    Mock::given(path("/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let catalog = catalog(&server);
    catalog.libraries().await.unwrap();
    catalog.unrestricted().identity().await.unwrap_err();
    let missing = catalog.item(&id(MediaKind::Movie, "404")).await;
    assert!(
        matches!(missing, Err(CatalogError::NotFound(_))),
        "{missing:?}"
    );
    let counts = catalog.request_counts();
    assert_eq!((counts.requests, counts.errors), (3, 1));
    assert_eq!(counts.buckets.last().unwrap().1, 3);
}

/// PX-09 finding 5 (B-2): a PMS that answers with a redirect to another
/// host does not get the token sent there; the redirect is not followed.
#[tokio::test]
async fn a_redirect_to_another_server_does_not_carry_the_token() {
    let elsewhere = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "MediaContainer": { "friendlyName": "Elsewhere" }
        })))
        .mount(&elsewhere)
        .await;
    let pms = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", format!("{}/", elsewhere.uri())),
        )
        .mount(&pms)
        .await;

    let catalog = PlexCatalog::new(pms.uri().parse().unwrap(), Some("s3cr3t".into())).unwrap();
    let _ = catalog.identity().await;

    let reached = elsewhere.received_requests().await.unwrap();
    let carried: Vec<_> = reached
        .iter()
        .filter(|r| r.headers.contains_key("x-plex-token"))
        .collect();
    assert!(carried.is_empty(), "the token reached {carried:?}");
}

/// A PMS on loopback that answers any request with a JSON body of `total`
/// bytes, sent as fast as it is read; it counts what it got to send.
async fn endless_pms(total: usize) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let sent = std::sync::Arc::new(AtomicUsize::new(0));
    let counted = sent.clone();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 4096];
        let _ = socket.read(&mut request).await;
        let head = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n";
        if socket.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        let chunk = vec![b' '; 1 << 20];
        let framed = [
            format!("{:x}\r\n", chunk.len()).into_bytes(),
            chunk,
            b"\r\n".to_vec(),
        ]
        .concat();
        while counted.load(Ordering::Relaxed) < total {
            if socket.write_all(&framed).await.is_err() {
                return;
            }
            counted.fetch_add(1 << 20, Ordering::Relaxed);
        }
        let _ = socket.write_all(b"0\r\n\r\n").await;
    });
    (url, sent)
}

/// PX-09 finding 26 (B-7): a PMS's answer was read whole before anything
/// looked at its size, so a malicious one could make the server hold as
/// much as it sent within the request's 15 s. Reading stops at the limit.
#[tokio::test]
async fn an_answer_past_the_limit_is_not_read_to_its_end() {
    let (url, sent) = endless_pms(512 << 20).await;
    let catalog = PlexCatalog::new(url.parse().unwrap(), None).unwrap();
    let err = catalog.server_name().await.unwrap_err();
    assert!(err.to_string().contains("over the limit"), "{err}");
    let sent = sent.load(std::sync::atomic::Ordering::Relaxed);
    assert!(sent < 256 << 20, "the PMS got to send {} MiB", sent >> 20);
}

/// The cast is on the item's own response (Plex's listings name only a
/// few people, by name alone), so it comes with the item's read and
/// costs no request of its own.
#[tokio::test]
async fn a_movie_comes_with_its_cast_from_its_own_response() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/206736", "metadata-movie-cast").await;
    let got = catalog(&server)
        .item(&id(MediaKind::Movie, "206736"))
        .await
        .unwrap();
    let Item::Movie(movie) = got.item else {
        panic!("not a movie");
    };
    let names: Vec<_> = movie.cast.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Tom Skerritt",
            "Sigourney Weaver",
            "Veronica Cartwright",
            "Harry Dean Stanton",
            "Ridley Scott"
        ]
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_library_s_items_have_no_cast() {
    let server = MockServer::start().await;
    serve(&server, "/library/sections/2/all", "watch-movies-watched").await;
    let page = catalog(&server)
        .items(
            &LibraryId::new(Namespace::Plex, "2").unwrap(),
            Page {
                offset: 0,
                limit: 2,
            },
        )
        .await
        .unwrap();
    for item in &page.items {
        let Item::Movie(movie) = item else {
            panic!("not a movie");
        };
        assert!(movie.cast.is_empty(), "{}", movie.title);
    }
}

/// A show's last airing is its last episode's: the last of its
/// `leafCount`, one episode asked for.
#[tokio::test]
async fn a_show_says_when_its_latest_episode_aired() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/208618", "metadata-show").await;
    serve(
        &server,
        "/library/metadata/208618/children",
        "children-show",
    )
    .await;
    Mock::given(path("/library/metadata/208618/allLeaves"))
        .and(header("X-Plex-Container-Start", "45"))
        .and(header("X-Plex-Container-Size", "1"))
        .respond_with(json("metadata-episode"))
        .expect(1)
        .mount(&server)
        .await;
    let got = catalog(&server)
        .item(&id(MediaKind::Show, "208618"))
        .await
        .unwrap();
    let Item::Show(show) = got.item else {
        panic!("not a show");
    };
    assert_eq!(
        show.last_aired,
        spoolframe_types::CalendarDate::new(2015, 1, 16)
    );
}

/// The last airing is extra: a show whose episodes cannot be listed
/// still loads, without it.
#[tokio::test]
async fn a_show_whose_episodes_cannot_be_listed_loads_without_its_last_airing() {
    let server = MockServer::start().await;
    serve(&server, "/library/metadata/208618", "metadata-show").await;
    serve(
        &server,
        "/library/metadata/208618/children",
        "children-show",
    )
    .await;
    Mock::given(path("/library/metadata/208618/allLeaves"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let got = catalog(&server)
        .item(&id(MediaKind::Show, "208618"))
        .await
        .unwrap();
    let Item::Show(show) = got.item else {
        panic!("not a show");
    };
    assert_eq!(show.last_aired, None);
    assert_eq!(got.children.len(), 2);
}

fn show_leaves(section: u32) -> ResponseTemplate {
    let episode = |key: &str, show: &str, season: u32, episode: u32| {
        serde_json::json!({
            "ratingKey": key,
            "type": "episode",
            "title": key,
            "grandparentRatingKey": show,
            "parentRatingKey": format!("{show}{season}"),
            "parentIndex": season,
            "index": episode,
        })
    };
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "MediaContainer": {
            "librarySectionID": section,
            "Metadata": [
                episode("11", "1", 0, 1),
                episode("21", "1", 1, 1),
                episode("22", "1", 1, 2),
                // Another show's: not this show's episode, whatever Plex
                // lists.
                episode("91", "9", 1, 1),
            ],
        }
    }))
}

/// A show's episodes, every season's and the specials, are one request.
#[tokio::test]
async fn a_show_s_episodes_are_its_all_leaves() {
    let server = MockServer::start().await;
    Mock::given(path("/library/metadata/1/allLeaves"))
        .respond_with(show_leaves(1))
        .expect(1)
        .mount(&server)
        .await;
    let episodes = catalog(&server)
        .episodes(&id(MediaKind::Show, "1"))
        .await
        .unwrap();
    let keys: Vec<&str> = episodes.iter().map(|e| e.id().key()).collect();
    assert_eq!(keys, ["11", "21", "22"]);
}

#[tokio::test]
async fn a_show_s_episodes_in_a_library_not_served_are_not_served() {
    let server = MockServer::start().await;
    Mock::given(path("/library/metadata/1/allLeaves"))
        .respond_with(show_leaves(2))
        .mount(&server)
        .await;
    let err = catalog(&server)
        .serving(Some(["1".to_owned()].into()))
        .episodes(&id(MediaKind::Show, "1"))
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotServed(_)), "{err}");
}

/// A person's photo is on Plex's people host; the PMS's photo
/// transcoder fetches it, so the token goes to the PMS alone.
#[tokio::test]
async fn a_person_s_photo_is_fetched_through_the_pms() {
    let server = MockServer::start().await;
    Mock::given(path("/photo/:/transcode"))
        .and(query_param(
            "url",
            "https://metadata-static.plex.tv/9/people/9e3c.jpg",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(b"\xff\xd8 fake".to_vec(), "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let image = catalog(&server)
        .artwork(&artwork_id("/metadata-static/9/people/9e3c.jpg"))
        .await
        .unwrap();
    assert_eq!(image.content_type, "image/jpeg");
    assert_eq!(image.bytes, b"\xff\xd8 fake");
}

#[tokio::test]
async fn a_person_photo_key_off_the_people_host_never_reaches_the_pms() {
    let server = MockServer::start().await;
    for key in [
        "/metadata-static/../library/sections",
        "/metadata-static/9/x.jpg?url=http://nas",
        "/metadata-static/",
    ] {
        let err = catalog(&server)
            .artwork(&artwork_id(key))
            .await
            .unwrap_err();
        assert!(matches!(err, CatalogError::NotFound(_)), "{key}: {err}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// PX-40: a playlist's entry finds its item again after a rebuild by
/// Plex's guid, asked of the served sections that hold its kind.
#[tokio::test]
async fn an_item_is_found_by_its_plex_guid_in_the_sections_of_its_kind() {
    let server = MockServer::start().await;
    serve(&server, "/library/sections", "sections").await;
    let guid = "plex://movie/5d776c85fb0d55001f589f07";
    Mock::given(method("GET"))
        .and(path("/library/sections/2/all"))
        .and(query_param("type", "1"))
        .and(query_param("guid", guid))
        .respond_with(json("metadata-movie"))
        .mount(&server)
        .await;
    let ids = ["imdb://tt0078748".to_owned(), guid.to_owned()];
    let found = catalog(&server)
        .find_external(MediaKind::Movie, &ids)
        .await
        .unwrap();
    assert_eq!(found, Some(id(MediaKind::Movie, "134883")));
}

#[tokio::test]
async fn an_item_no_section_has_is_not_found_by_its_guid() {
    let server = MockServer::start().await;
    serve(&server, "/library/sections", "sections").await;
    Mock::given(method("GET"))
        .and(path("/library/sections/2/all"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(r#"{"MediaContainer":{"size":0}}"#, "application/json"),
        )
        .mount(&server)
        .await;
    let plex = catalog(&server);
    let gone = ["plex://movie/0000000000000000000000ff".to_owned()];
    assert_eq!(
        plex.find_external(MediaKind::Movie, &gone).await.unwrap(),
        None
    );
    // Agents' ids alone are not asked about.
    let agents = ["imdb://tt0078748".to_owned()];
    let before = server.received_requests().await.unwrap().len();
    assert_eq!(
        plex.find_external(MediaKind::Movie, &agents).await.unwrap(),
        None
    );
    assert_eq!(server.received_requests().await.unwrap().len(), before);
}

/// PX-40's import: an account's video and music playlists, photos left
/// out, and what each holds as items of this catalog, with what is not.
#[tokio::test]
async fn an_account_s_playlists_and_their_items_are_read_with_what_does_not_map() {
    let server = MockServer::start().await;
    let body = |value: serde_json::Value| {
        ResponseTemplate::new(200).set_body_raw(value.to_string(), "application/json")
    };
    Mock::given(method("GET"))
        .and(path("/playlists"))
        .and(header("X-Plex-Token", "alice-token"))
        .respond_with(body(serde_json::json!({ "MediaContainer": { "Metadata": [
            { "ratingKey": "500", "type": "playlist", "title": "Heists",
              "playlistType": "video", "smart": false },
            { "ratingKey": "501", "type": "playlist", "title": "Holiday",
              "playlistType": "photo", "smart": false },
            { "ratingKey": "502", "type": "playlist", "title": "Recent",
              "playlistType": "audio", "smart": true },
        ] } })))
        .mount(&server)
        .await;
    let movie = serde_json::from_str::<serde_json::Value>(&fixture("metadata-movie")).unwrap()
        ["MediaContainer"]["Metadata"][0]
        .clone();
    Mock::given(method("GET"))
        .and(path("/playlists/500/items"))
        .respond_with(body(serde_json::json!({ "MediaContainer": { "Metadata": [
            movie,
            { "ratingKey": "77", "type": "clip", "title": "A trailer" },
        ] } })))
        .mount(&server)
        .await;
    let plex = PlexCatalog::new(server.uri().parse().unwrap(), Some("alice-token".into())).unwrap();
    let lists = plex.playlists().await.unwrap();
    let names: Vec<_> = lists.iter().map(|l| (l.title.as_str(), l.smart)).collect();
    assert_eq!(names, [("Heists", false), ("Recent", true)]);
    let items = plex.playlist_items("500").await.unwrap();
    assert!(
        matches!(&items[0], spoolframe_plex::playlists::PlexPlaylistItem::Item(item)
            if *item.id() == id(MediaKind::Movie, "134883")),
        "{items:?}"
    );
    assert!(matches!(
        &items[1],
        spoolframe_plex::playlists::PlexPlaylistItem::Unmapped { title, .. } if title == "A trailer"
    ));
}

/// PX-53: a playlist's own picture is its `thumb`; Plex's composite of
/// what it holds is not one, whether or not `thumb` names it too. The
/// picture is fetched through the photo transcoder, at most 2048 a side.
#[tokio::test]
async fn a_playlist_s_own_picture_is_told_from_plex_s_composite() {
    let server = MockServer::start().await;
    let body = |value: serde_json::Value| {
        ResponseTemplate::new(200).set_body_raw(value.to_string(), "application/json")
    };
    Mock::given(method("GET"))
        .and(path("/playlists"))
        .respond_with(body(serde_json::json!({ "MediaContainer": { "Metadata": [
            { "ratingKey": "500", "type": "playlist", "title": "Own", "playlistType": "video",
              "thumb": "/library/metadata/500/thumb/17", "composite": "/playlists/500/composite/17" },
            { "ratingKey": "501", "type": "playlist", "title": "Composite", "playlistType": "audio",
              "composite": "/playlists/501/composite/17" },
            { "ratingKey": "502", "type": "playlist", "title": "Thumb is composite",
              "playlistType": "audio", "thumb": "/playlists/502/composite/17",
              "composite": "/playlists/502/composite/17" },
            { "ratingKey": "503", "type": "playlist", "title": "Elsewhere", "playlistType": "audio",
              "thumb": "https://example.com/x.jpg" },
        ] } })))
        .mount(&server)
        .await;
    Mock::given(path("/photo/:/transcode"))
        .and(query_param("url", "/library/metadata/500/thumb/17"))
        .and(query_param("width", "2048"))
        .and(query_param("height", "2048"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(b"\xff\xd8 fake".to_vec(), "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let plex = catalog(&server);
    let lists = plex.playlists().await.unwrap();
    let art: Vec<_> = lists.iter().map(|l| l.art.as_deref()).collect();
    assert_eq!(
        art,
        [Some("/library/metadata/500/thumb/17"), None, None, None]
    );
    let image = plex
        .playlist_art("/library/metadata/500/thumb/17")
        .await
        .unwrap();
    assert_eq!(image.content_type, "image/jpeg");
}

/// PX-43's All: a music section's tracks are one listing of its leaves,
/// page by page, not a request per album.
#[tokio::test]
async fn a_music_library_s_tracks_are_one_listing() {
    let server = MockServer::start().await;
    Mock::given(path("/library/sections/12/all"))
        .and(query_param("type", "10"))
        .respond_with(json("section-leaves-track"))
        .expect(1)
        .mount(&server)
        .await;
    let music = LibraryId::new(Namespace::Plex, "12").unwrap();
    let tracks = catalog(&server).tracks(&music).await.unwrap();
    let keys: Vec<&str> = tracks.iter().map(|t| t.id().key()).collect();
    assert_eq!(keys, ["228207", "228208"]);
    assert!(tracks.iter().all(|t| matches!(t, Item::Track(_))));
    let unserved = catalog(&server)
        .serving(Some(["2".to_owned()].into()))
        .tracks(&music)
        .await
        .unwrap_err();
    assert!(matches!(unserved, CatalogError::NotServed(_)), "{unserved}");
}

/// PX-43's import: the tracks the account rated, asked of each music
/// section by `userRating>>=0`, the last rated first; one Plex sends
/// unrated anyway is left out.
#[tokio::test]
async fn the_rated_tracks_are_the_last_rated_first() {
    let server = MockServer::start().await;
    serve(&server, "/library/sections", "sections").await;
    let track = |key: &str, rating: f64, at: i64| {
        serde_json::json!({ "ratingKey": key, "type": "track", "title": key,
            "parentRatingKey": "40", "grandparentRatingKey": "39",
            "userRating": rating, "lastRatedAt": at })
    };
    Mock::given(path("/library/sections/12/all"))
        .and(query_param("type", "10"))
        .and(query_param("userRating>>", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "MediaContainer": { "totalSize": 3, "Metadata": [
                track("41", 10.0, 100), track("42", 0.0, 300), track("43", 4.0, 200),
            ] }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let rated = catalog(&server).rated_tracks().await.unwrap();
    let keys: Vec<&str> = rated.iter().map(|t| t.id().key()).collect();
    assert_eq!(keys, ["43", "41"]);
}

/// The thumb of an item whose art an agent gave, as a PMS answers it: a
/// redirect to `location`. The item is the fixture's film.
async fn redirected_thumb(pms: &MockServer, location: &str) -> spoolframe_types::ArtworkId {
    serve(pms, "/library/metadata/134883", "metadata-movie").await;
    Mock::given(path("/library/metadata/134883/thumb/1792415642"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", location))
        .mount(pms)
        .await;
    artwork_id("/library/metadata/134883/thumb/1792415642")
}

/// A server that answers `/photo` with a JPEG, as Plex's image proxy does.
async fn image_proxy() -> MockServer {
    let proxy = MockServer::start().await;
    Mock::given(path("/photo"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(b"\xff\xd8 proxied".to_vec(), "image/jpeg"),
        )
        .mount(&proxy)
        .await;
    proxy
}

/// A PMS sends an agent's art to Plex's image proxy (images.plex.tv) with
/// a 302: the image is fetched there, and the token stays with the PMS.
#[tokio::test]
async fn artwork_redirected_to_plex_s_image_proxy_is_fetched_without_the_token() {
    let pms = MockServer::start().await;
    let proxy = image_proxy().await;
    let thumb = redirected_thumb(
        &pms,
        &format!(
            "{}/photo?height=480&url=https%3A%2F%2Fmetadata-static.plex.tv%2F1%2Fa.jpg",
            proxy.uri()
        ),
    )
    .await;
    let catalog = PlexCatalog::new(pms.uri().parse().unwrap(), Some("s3cr3t".into()))
        .unwrap()
        .reaching_inner_hosts();

    let image = catalog.artwork(&thumb).await.unwrap();

    assert_eq!(
        (image.content_type.as_str(), image.bytes.as_slice()),
        ("image/jpeg", b"\xff\xd8 proxied".as_slice())
    );
    let reached = proxy.received_requests().await.unwrap();
    assert_eq!(reached.len(), 1);
    assert!(
        !reached[0].headers.contains_key("x-plex-token")
            && !reached[0].url.as_str().contains("s3cr3t"),
        "the token reached the proxy: {:?}",
        reached[0]
    );
}

/// A redirect within the PMS is followed with the token, as the PMS
/// asks for it on every path.
#[tokio::test]
async fn artwork_redirected_within_the_pms_keeps_the_token() {
    let pms = MockServer::start().await;
    let thumb = redirected_thumb(&pms, "/library/metadata/134883/thumb/2").await;
    Mock::given(path("/library/metadata/134883/thumb/2"))
        .and(header("X-Plex-Token", "s3cr3t"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(b"\x89PNG moved".to_vec(), "image/png"),
        )
        .expect(1)
        .mount(&pms)
        .await;
    let catalog = PlexCatalog::new(pms.uri().parse().unwrap(), Some("s3cr3t".into())).unwrap();

    let image = catalog.artwork(&thumb).await.unwrap();

    assert_eq!(image.bytes, b"\x89PNG moved");
}

/// A redirect off the PMS goes only to a public host over HTTPS: one to
/// an inner host, or in the clear, is refused before anything is sent to
/// it, and the error says why.
#[tokio::test]
async fn artwork_redirected_off_the_pms_to_an_inner_host_is_refused() {
    let proxy = image_proxy().await;
    let port = proxy.address().port();
    for (location, why) in [
        (format!("{}/photo", proxy.uri()), "not HTTPS"),
        (format!("https://127.0.0.1:{port}/photo"), "loopback"),
        ("https://10.1.2.3/photo".to_owned(), "private"),
        ("https://[fe80::1]/photo".to_owned(), "link-local"),
        ("https://169.254.169.254/latest".to_owned(), "link-local"),
        (format!("https://localhost:{port}/photo"), "local name"),
        ("https://nas.lan/photo".to_owned(), "local name"),
    ] {
        let pms = MockServer::start().await;
        let thumb = redirected_thumb(&pms, &location).await;
        let err = catalog(&pms).artwork(&thumb).await.unwrap_err();
        assert!(
            matches!(err, CatalogError::Backend { .. }) && err.to_string().contains(why),
            "{location}: {err}"
        );
    }
    assert!(proxy.received_requests().await.unwrap().is_empty());
}

/// A redirect off the PMS that would carry the token in its URL is
/// refused: the token is the PMS's alone.
#[tokio::test]
async fn artwork_redirected_off_the_pms_with_the_token_in_its_url_is_refused() {
    let pms = MockServer::start().await;
    let proxy = image_proxy().await;
    let thumb = redirected_thumb(
        &pms,
        &format!("{}/photo?url=%2Fx%3FX-Plex-Token%3Ds3cr3t", proxy.uri()),
    )
    .await;
    let catalog = PlexCatalog::new(pms.uri().parse().unwrap(), Some("s3cr3t".into()))
        .unwrap()
        .reaching_inner_hosts();

    let err = catalog.artwork(&thumb).await.unwrap_err();

    assert!(err.to_string().contains("token"), "{err}");
    assert!(proxy.received_requests().await.unwrap().is_empty());
}

/// At most three redirects are followed: a PMS that keeps redirecting is
/// asked four times for the image, and then it is an error.
#[tokio::test]
async fn artwork_past_three_redirects_is_an_error() {
    let pms = MockServer::start().await;
    let thumb = redirected_thumb(&pms, "/library/metadata/134883/thumb/1792415642").await;

    let err = catalog(&pms).artwork(&thumb).await.unwrap_err();

    let asked = pms
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/thumb/1792415642"))
        .count();
    assert_eq!(asked, 4, "{err}");
    assert!(err.to_string().contains("redirects"), "{err}");
}
