//! The site's calls against a stand-in on loopback (wiremock), answering
//! as the site documents: never the real API.

use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const SEARCH: &str = include_str!("../../tests/fixtures/opensubtitles/search.json");

fn account(server: &MockServer, login: bool) -> Account {
    Account {
        api_url: url::Url::parse(&format!("{}/api/v1", server.uri())).unwrap(),
        api_key: "test-key".into(),
        username: login.then(|| "alice".into()),
        password: login.then(|| "pw".into()),
    }
}

#[tokio::test]
async fn a_search_sends_the_key_and_its_query_in_order_and_reads_each_file() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/subtitles"))
        .and(header("Api-Key", "test-key"))
        .and(query_param("moviehash", "00000000000200ff"))
        .and(query_param("imdb_id", "1234567"))
        .and(query_param("languages", "en,pt-br"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SEARCH))
        .expect(1)
        .mount(&server)
        .await;
    let site = Site::new(account(&server, false));
    let found = site
        .search(&Query {
            moviehash: Some("00000000000200FF".into()),
            imdb_id: Some(1_234_567),
            languages: "en,pt-BR".into(),
            ..Query::default()
        })
        .await
        .unwrap();
    assert_eq!(
        found.iter().map(|f| f.file_id).collect::<Vec<_>>(),
        [101, 102, 103, 104]
    );
    assert!(found[0].hash_match);
    assert_eq!(found[0].fps, Some(23.976));
    assert_eq!(
        found[0].file_name.as_deref(),
        Some("Film.2019.1080p.BluRay.x264-GRP.srt")
    );
    assert!(found[1].hearing_impaired);
    assert!(found[2].machine_translated && found[2].fps.is_none());
    assert!(found[3].ai_translated);
    // The query string is in key order, as the site wants it.
    let asked = &server.received_requests().await.unwrap()[0];
    let keys: Vec<String> = asked
        .url
        .query_pairs()
        .map(|(k, _)| k.into_owned())
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

/// Told to wait, the next call is not made until then: the site is not
/// asked again.
#[tokio::test]
async fn told_to_wait_it_waits() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "30"))
        .expect(1)
        .mount(&server)
        .await;
    let site = Site::new(account(&server, false));
    let query = Query {
        query: Some("film".into()),
        languages: "en".into(),
        ..Query::default()
    };
    assert_eq!(
        site.search(&query).await,
        Err(SiteError::RateLimited {
            retry_after_ms: 30_000
        })
    );
    assert!(matches!(
        site.search(&query).await,
        Err(SiteError::RateLimited { retry_after_ms }) if retry_after_ms <= 30_000
    ));
}

#[tokio::test]
async fn calls_are_spaced_out() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
        .mount(&server)
        .await;
    let site = Site::new(account(&server, false));
    let query = Query {
        languages: "en".into(),
        ..Query::default()
    };
    let started = Instant::now();
    for _ in 0..3 {
        site.search(&query).await.unwrap();
    }
    assert!(
        started.elapsed() >= MIN_INTERVAL * 2,
        "{:?}",
        started.elapsed()
    );
}

/// With an account, a download signs in once and carries the token; the
/// file comes from the link, and the account's count with it.
#[tokio::test]
async fn a_download_signs_in_and_fetches_the_link() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/login"))
        .and(body_json(
            serde_json::json!({ "username": "alice", "password": "pw" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "user": { "allowed_downloads": 20 },
            "token": "tok",
            "base_url": "api.opensubtitles.com",
            "status": 200
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/download"))
        .and(header("Authorization", "Bearer tok"))
        .and(body_json(serde_json::json!({ "file_id": 101 })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "link": format!("{}/files/101.srt", server.uri()),
            "file_name": "x.srt",
            "requests": 1,
            "remaining": 19,
            "message": "",
            "reset_time": "23 hours",
            "reset_time_utc": "2022-04-08T13:03:16.000Z"
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/101.srt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("subtitle"))
        .mount(&server)
        .await;
    let site = Site::new(account(&server, true));
    let got = site.download(101, None).await.unwrap();
    assert_eq!(got.bytes, b"subtitle");
    assert_eq!(got.remaining, Some(19));
    assert_eq!(got.resets_at, Some(1_649_422_996));
    // The token is kept: no second sign-in.
    site.download(101, None).await.unwrap();
}

#[tokio::test]
async fn an_exhausted_account_says_when_it_resets() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/download"))
        .respond_with(ResponseTemplate::new(406).set_body_json(serde_json::json!({
            "requests": 6,
            "remaining": 0,
            "message": "You have downloaded your allowed 5 subtitles for 24h.",
            "reset_time_utc": "2022-04-08T13:03:16.000Z"
        })))
        .mount(&server)
        .await;
    let site = Site::new(account(&server, false));
    assert_eq!(
        site.download(101, Some("srt")).await,
        Err(SiteError::QuotaExhausted {
            resets_at: Some(1_649_422_996)
        })
    );
}

/// A link to anywhere but the site is not followed.
#[tokio::test]
async fn a_link_away_from_the_site_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "link": "http://169.254.169.254/latest/meta-data",
            "remaining": 3
        })))
        .mount(&server)
        .await;
    let site = Site::new(account(&server, false));
    assert!(matches!(
        site.download(101, None).await,
        Err(SiteError::Unreadable(_))
    ));
}

#[tokio::test]
async fn a_file_past_the_cap_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "link": format!("{}/files/big.srt", server.uri()),
        })))
        .mount(&server)
        .await;
    let big = vec![b'a'; spoolframe_media::subtitle_file::MAX_DOWNLOAD_BYTES + 1];
    Mock::given(method("GET"))
        .and(path("/files/big.srt"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(big))
        .mount(&server)
        .await;
    let site = Site::new(account(&server, false));
    assert!(matches!(
        site.download(101, None).await,
        Err(SiteError::TooLarge(_))
    ));
}

#[tokio::test]
async fn a_wrong_key_is_refused_with_the_site_s_word() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(serde_json::json!({ "message": "You cannot consume this service" })),
        )
        .mount(&server)
        .await;
    let site = Site::new(account(&server, false));
    let refused = site
        .search(&Query {
            languages: "en".into(),
            ..Query::default()
        })
        .await;
    assert_eq!(
        refused,
        Err(SiteError::Refused {
            status: 403,
            message: "You cannot consume this service".into()
        })
    );
}

/// A loopback address nothing listens on: a port just let go of. (A
/// dropped wiremock server goes back to its pool, still listening.)
pub(crate) fn nothing_listens() -> url::Url {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    url::Url::parse(&format!("http://127.0.0.1:{port}/api/v1")).unwrap()
}

#[tokio::test]
async fn an_unreachable_site_is_said_so() {
    let site = Site::new(Account {
        api_url: nothing_listens(),
        api_key: "test-key".into(),
        username: None,
        password: None,
    });
    assert!(matches!(
        site.search(&Query::default()).await,
        Err(SiteError::Unreachable(_))
    ));
}

#[test]
fn a_utc_time_is_read_and_a_wrong_one_is_not() {
    assert_eq!(utc_seconds("2022-04-08T13:03:16.000Z"), Some(1_649_422_996));
    assert_eq!(utc_seconds("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(utc_seconds("2022-13-08T13:03:16Z"), None);
    assert_eq!(utc_seconds("in 7 hours"), None);
    assert_eq!(utc_seconds("2022-04-08T13:03"), None);
    assert_eq!(utc_seconds("+022-04-08T13:03:16Z"), None);
}

/// The hash is the size plus the 64-bit words of the first and last
/// 64 KiB; a file too small for both is not hashed.
#[tokio::test]
async fn the_movie_hash_adds_the_size_and_both_ends() {
    let dir = spoolframe_test_dir::TestDir::new("spoolframe-moviehash").await;
    let path = dir.as_ref().join("film.mkv");
    let mut bytes = vec![0u8; 3 * 64 * 1024];
    bytes[0] = 1;
    // In the middle, read by neither end.
    bytes[100_000] = 0xff;
    let last = bytes.len() - 8;
    bytes[last] = 2;
    std::fs::write(&path, &bytes).unwrap();
    let expected = format!("{:016x}", bytes.len() as u64 + 3);
    assert_eq!(movie_hash(&path).unwrap(), Some(expected));
    std::fs::write(&path, vec![0u8; 100]).unwrap();
    assert_eq!(movie_hash(&path).unwrap(), None);
}

#[test]
fn only_the_site_s_own_api_hosts_are_moved_to() {
    let api = url::Url::parse(API_URL).unwrap();
    assert_eq!(
        moved_api(&api, "vip-api.opensubtitles.com").map(|u| u.to_string()),
        Some("https://vip-api.opensubtitles.com/api/v1".into())
    );
    assert_eq!(moved_api(&api, "evil.example"), None);
    assert_eq!(moved_api(&api, "opensubtitles.com.evil.example"), None);
}

#[test]
fn an_account_s_secrets_are_never_printed() {
    let account = Account {
        api_url: url::Url::parse(API_URL).unwrap(),
        api_key: "s3cret-key".into(),
        username: Some("alice".into()),
        password: Some("s3cret-pw".into()),
    };
    let shown = format!("{account:?}");
    assert!(!shown.contains("s3cret"), "{shown}");
}
