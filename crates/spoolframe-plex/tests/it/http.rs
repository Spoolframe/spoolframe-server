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
                start_ms: 258_865,
                end_ms: 319_865,
                to_end: false,
            },
            Marker {
                kind: MarkerKind::Credits,
                start_ms: 2_815_419,
                // 1 ms past the episode's duration, as Plex has it.
                end_ms: 2_852_689,
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
            start_ms: 30_000,
            end_ms: 90_000,
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
