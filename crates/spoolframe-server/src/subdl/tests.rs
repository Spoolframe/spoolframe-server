//! SubDL's calls against a stand-in on loopback (wiremock), answering as
//! SubDL does (a recorded search, its key replaced): never the real API.

use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

pub(crate) const SEARCH: &str = include_str!("../../tests/fixtures/subdl/search.json");

fn account(server: &MockServer) -> Account {
    Account {
        api_url: url::Url::parse(&format!("{}/api/v1", server.uri())).unwrap(),
        download_url: url::Url::parse(&server.uri()).unwrap(),
        api_key: "test-key".into(),
    }
}

fn film() -> Query {
    Query {
        imdb_id: Some("tt1234567".into()),
        kind: "movie",
        languages: "EN,BR_PT".into(),
        ..Query::default()
    }
}

/// The key and the query go in the query string; each result is read
/// for its path alone, the key SubDL wrote into its link left behind; a
/// link that is not SubDL's shape is not a result.
#[tokio::test]
async fn a_search_reads_each_result_without_the_key_in_its_link() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/subtitles"))
        .and(query_param("api_key", "test-key"))
        .and(query_param("imdb_id", "tt1234567"))
        .and(query_param("type", "movie"))
        .and(query_param("languages", "EN,BR_PT"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SEARCH))
        .expect(1)
        .mount(&server)
        .await;
    let site = Site::new(account(&server));
    let found = site.search(&film()).await.unwrap();
    assert_eq!(
        found.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
        ["3172974-3188291", "2978466-2972819", "504692-1185"]
    );
    assert_eq!(found[0].path, "/subtitle/3172974-3188291.zip");
    assert_eq!(found[0].format, None);
    assert_eq!(found[0].fps, Some(23.976));
    assert_eq!(found[0].release, "Film.2019.1080p.BluRay.x264-GRP");
    assert_eq!(found[2].language, "pt-br");
    assert_eq!(found[2].format.as_deref(), Some("srt"));
    assert!(
        !format!("{found:?}").contains("FIXTUREKEY"),
        "the key SubDL echoes is kept nowhere"
    );
}

/// SubDL's own words for nothing found are no result; any other
/// `status: false`, even one that says "not found", is a refusal.
#[tokio::test]
async fn nothing_found_is_no_result_and_a_used_up_key_is_said() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("imdb_id", "tt1234567"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": false, "error": "can't find movie or tv",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("imdb_id", "tt7654321"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": false, "error": "API key not found",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("film_name", "film"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "status": false, "error": "Daily request limit exceeded",
        })))
        .mount(&server)
        .await;
    let site = Site::new(account(&server));
    assert_eq!(site.search(&film()).await.unwrap(), Vec::new());
    let bad_key = Query {
        imdb_id: Some("tt7654321".into()),
        ..film()
    };
    assert!(matches!(
        site.search(&bad_key).await,
        Err(SiteError::Refused { status: 200, .. })
    ));
    let by_name = Query {
        film_name: Some("film".into()),
        kind: "movie",
        languages: "EN".into(),
        ..Query::default()
    };
    assert_eq!(
        site.search(&by_name).await,
        Err(SiteError::QuotaExhausted { resets_at: None })
    );
}

/// Told to wait, the next call is not made until then.
#[tokio::test]
async fn told_to_wait_it_waits() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "30"))
        .expect(1)
        .mount(&server)
        .await;
    let site = Site::new(account(&server));
    assert_eq!(
        site.search(&film()).await,
        Err(SiteError::RateLimited {
            retry_after_ms: 30_000
        })
    );
    assert!(matches!(
        site.search(&film()).await,
        Err(SiteError::RateLimited { retry_after_ms }) if retry_after_ms <= 30_000
    ));
}

/// A download is the result's path on the download host, the key in a
/// header, never in the URL.
#[tokio::test]
async fn a_download_is_the_path_with_the_key_in_a_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/subtitle/3172974-3188291.zip"))
        .and(header("x-api-key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"PK\x03\x04zip".to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    let site = Site::new(account(&server));
    let got = site
        .download("/subtitle/3172974-3188291.zip")
        .await
        .unwrap();
    assert_eq!(got, b"PK\x03\x04zip");
    let asked = &server.received_requests().await.unwrap()[0];
    assert_eq!(asked.url.query(), None);
    // Anything but SubDL's shape of a path is never asked for.
    for bad in [
        "/subtitle/../../admin",
        "/other/1-2.zip",
        "/subtitle/1-2.zip?x=y",
        "//elsewhere.example/1-2.zip",
    ] {
        assert!(
            matches!(site.download(bad).await, Err(SiteError::Unreadable(_))),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn a_download_past_the_cap_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![
            0u8;
            spoolframe_media::subtitle_file::MAX_ARCHIVE_BYTES
                + 1
        ]))
        .mount(&server)
        .await;
    let site = Site::new(account(&server));
    assert!(matches!(
        site.download("/subtitle/1-2.zip").await,
        Err(SiteError::TooLarge(_))
    ));
}

#[test]
fn languages_are_named_as_subdl_and_the_server_name_them() {
    assert_eq!(code_of("en"), "EN");
    assert_eq!(code_of("pt-br"), "BR_PT");
    assert_eq!(code_of("zh-tw"), "ZH_BG");
    assert_eq!(code_of("fr-ca"), "FR");
    assert_eq!(language_of("EN").as_deref(), Some("en"));
    assert_eq!(language_of("BR_PT").as_deref(), Some("pt-br"));
    assert_eq!(language_of("ZH_BG").as_deref(), Some("zh-tw"));
    assert_eq!(language_of("../x"), None);
}
