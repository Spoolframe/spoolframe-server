//! Searching, downloading and removing against a stand-in site on
//! loopback (wiremock) answering with recorded responses: never the real
//! API.

use spoolframe_media::sandbox::Tool;
use spoolframe_types::{MediaKind, Namespace, SubtitleSync};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::principal::Principal;
use crate::store::tests::{TestStore, test_store};
use crate::store::users::LocalUserId;
use crate::subtitle_sync::{FfmpegSpeech, SyncSettings};
use crate::test_support::{FakeCatalog, fake_ffprobe, movie};

const SEARCH: &str = include_str!("../../tests/fixtures/opensubtitles/search.json");

/// An SRT in CP-1251, as many old Russian subtitles are.
const RUSSIAN: &str = "1\r\n00:00:01,000 --> 00:00:02,500\r\nПривет, мир\r\n\r\n\
                       2\r\n00:00:03,000 --> 00:00:04,000\r\nКак дела?\r\n";

fn asker(n: u128, admin: bool, name: &str) -> Asker {
    Asker {
        actor: Actor {
            principal: Principal::LocalUser(LocalUserId::from_uuid(uuid::Uuid::from_u128(n))),
            admin,
        },
        name: name.into(),
    }
}

fn alice() -> Asker {
    asker(1, false, "Alice")
}

fn bob() -> Asker {
    asker(2, false, "Bob")
}

fn root() -> Asker {
    asker(3, true, "Root")
}

struct Fixture {
    downloads: SubtitleDownloads,
    catalog: Arc<FakeCatalog>,
    prober: Arc<Prober>,
    timings: Arc<SubtitleTimings>,
    site: MockServer,
    media: PathBuf,
    store: TestStore,
    _tmp: spoolframe_test_dir::TestDir,
}

fn settings(site: &MockServer, limit: u32) -> SiteSettings {
    SiteSettings {
        account: Account {
            api_url: url::Url::parse(&format!("{}/api/v1", site.uri())).unwrap(),
            api_key: "test-key".into(),
            username: None,
            password: None,
        },
        languages: vec!["en".into()],
        daily_limit_per_user: limit,
    }
}

async fn fixture(test: &str, limit: u32) -> Fixture {
    let tmp = spoolframe_test_dir::TestDir::new(&format!("spoolframe-subdl-{test}")).await;
    let dir = tmp.path().to_path_buf();
    let store = test_store(&format!("subdl-{test}")).await;
    let media = dir.join("media").join("Alien.mkv");
    std::fs::create_dir_all(media.parent().unwrap()).unwrap();
    // Large enough to be hashed: 128 KiB and more.
    std::fs::write(&media, vec![7u8; 200 * 1024]).unwrap();
    let catalog = Arc::new(FakeCatalog::new(media.clone()));
    let prober = Arc::new(Prober::new(
        Tool::unconfined(fake_ffprobe(&dir, "mkv-forced-subs.json")),
        Duration::from_secs(10),
    ));
    let timings = Arc::new(
        SubtitleTimings::new(
            store.clone(),
            Tool::unconfined("/nonexistent/ffmpeg"),
            Arc::new(FfmpegSpeech(Tool::unconfined("/nonexistent/ffmpeg"))),
            &SyncSettings {
                at_once: 1,
                dir: dir.join("state").join("subtitles"),
                speech_dir: dir.join("state").join("speech"),
            },
        )
        .unwrap(),
    );
    let site = MockServer::start().await;
    let downloads = SubtitleDownloads::new(
        store.clone(),
        catalog.clone(),
        prober.clone(),
        timings.clone(),
        dir.join("state").join("subtitles").join("downloaded"),
        Some(settings(&site, limit)),
    )
    .unwrap();
    Fixture {
        downloads,
        catalog,
        prober,
        timings,
        site,
        media,
        store,
        _tmp: tmp,
    }
}

impl Fixture {
    async fn answers_search(&self) {
        Mock::given(method("GET"))
            .and(path("/api/v1/subtitles"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SEARCH))
            .mount(&self.site)
            .await;
    }

    /// Serves file `file_id` as `bytes`, the account then having
    /// `remaining` downloads.
    async fn serves(&self, file_id: u64, bytes: Vec<u8>, remaining: i64) {
        Mock::given(method("POST"))
            .and(path("/api/v1/download"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({ "file_id": file_id }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "link": format!("{}/files/{file_id}", self.site.uri()),
                "remaining": remaining,
                "reset_time_utc": "2030-01-01T00:00:00.000Z"
            })))
            .mount(&self.site)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/files/{file_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .mount(&self.site)
            .await;
    }

    async fn subtitles(&self) -> Vec<SubtitleStream> {
        self.prober
            .describe(&self.media)
            .await
            .unwrap()
            .subtitles
            .clone()
    }

    async fn site_calls(&self, to: &str) -> usize {
        self.site
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == to)
            .count()
    }
}

fn film() -> MediaId {
    movie().id().clone()
}

/// Hash matches first; then by rating and downloads; machine-translated
/// and AI-made after people's, each marked.
#[tokio::test]
async fn results_come_hash_matches_first_and_machine_made_last() {
    let f = fixture("order", 0).await;
    f.answers_search().await;
    let found = f.downloads.search(&film(), None).await.unwrap();
    assert_eq!(found.languages, ["en"]);
    let ids: Vec<u64> = found.results.iter().map(|r| r.file_id).collect();
    assert_eq!(ids, [101, 102, 103, 104]);
    assert!(found.results[0].hash_match);
    assert_eq!(found.results[1].rating, Some(91));
    assert_eq!(found.results[1].format.as_deref(), Some("ass"));
    assert_eq!(found.results[2].origin, SubtitleOrigin::MachineTranslated);
    assert_eq!(found.results[3].origin, SubtitleOrigin::Ai);
    assert_eq!(found.results[0].fps_milli, Some(23_976));
}

/// Opening the menu twice asks the site once; a day later, again.
#[tokio::test]
async fn a_search_is_kept_a_day() {
    let f = fixture("kept", 0).await;
    f.answers_search().await;
    f.downloads.search(&film(), None).await.unwrap();
    f.downloads.search(&film(), Some("EN")).await.unwrap();
    assert_eq!(f.site_calls("/api/v1/subtitles").await, 1);
    f.store.clock().advance(SEARCH_LIFE);
    f.downloads.search(&film(), None).await.unwrap();
    assert_eq!(f.site_calls("/api/v1/subtitles").await, 2);
}

/// A film without ids is searched by its hash, title and year; one with
/// an IMDb id by the hash and that id.
#[tokio::test]
async fn a_film_is_searched_by_its_hash_and_id_or_title() {
    let f = fixture("query", 0).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/subtitles"))
        .and(query_param("query", "alien"))
        .and(query_param("year", "1979"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
        .expect(1)
        .mount(&f.site)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/subtitles"))
        .and(query_param("imdb_id", "78748"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
        .expect(1)
        .mount(&f.site)
        .await;
    f.downloads.search(&film(), Some("en")).await.unwrap();
    f.catalog
        .give_ids(&film(), &["imdb://tt0078748", "tmdb://348"]);
    f.downloads.search(&film(), Some("fr")).await.unwrap();
    let asked = f.site.received_requests().await.unwrap();
    let hash = opensubtitles::movie_hash(&f.media).unwrap().unwrap();
    for request in &asked {
        let pairs: HashMap<_, _> = request.url.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("moviehash"), Some(&hash));
    }
    let by_id: HashMap<_, _> = asked[1].url.query_pairs().into_owned().collect();
    assert!(!by_id.contains_key("tmdb_id") && !by_id.contains_key("query"));
}

/// An episode with no id of its own is searched by its show's, with the
/// season and episode.
#[tokio::test]
async fn an_episode_is_searched_by_its_show_s_id_with_its_numbers() {
    let f = fixture("episode", 0).await;
    let show = MediaId::new(Namespace::Plex, MediaKind::Show, "10").unwrap();
    let episode = MediaId::new(Namespace::Plex, MediaKind::Episode, "15").unwrap();
    crate::test_support::write_episode(&f.media, "15");
    f.catalog.give_ids(&show, &["tmdb://63639"]);
    Mock::given(method("GET"))
        .and(path("/api/v1/subtitles"))
        .and(query_param("parent_tmdb_id", "63639"))
        .and(query_param("season_number", "2"))
        .and(query_param("episode_number", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
        .expect(1)
        .mount(&f.site)
        .await;
    f.downloads.search(&episode, None).await.unwrap();
}

#[tokio::test]
async fn a_bad_language_is_refused_before_the_site_is_asked() {
    let f = fixture("language", 0).await;
    assert!(matches!(
        f.downloads.search(&film(), Some("en&imdb_id=1")).await,
        Err(DownloadError::InvalidInput(_))
    ));
    assert_eq!(f.site_calls("/api/v1/subtitles").await, 0);
}

/// A CP-1251 SRT is kept as UTF-8, is a track of the film at once for
/// everyone, after the file's own; the same result again is the same
/// track, at no cost.
#[tokio::test]
async fn a_download_is_a_track_at_once_and_only_once() {
    let f = fixture("download", 0).await;
    f.answers_search().await;
    let (cp1251, _, _) = encoding_rs::WINDOWS_1251.encode(RUSSIAN);
    f.serves(103, cp1251.into_owned(), 19).await;
    let own = f.subtitles().await.len();
    f.downloads.search(&film(), None).await.unwrap();
    let got = f.downloads.download(&film(), 103, &alice()).await.unwrap();
    assert_eq!(got.track as usize, own);
    assert_eq!(got.account_remaining, Some(19));
    assert_eq!(got.user_remaining, None);
    let download = got.subtitle.download.clone().unwrap();
    assert_eq!(
        (download.by.as_str(), download.origin, download.can_remove),
        ("Alice", SubtitleOrigin::MachineTranslated, true)
    );
    assert!(got.subtitle.external);
    assert_eq!(got.subtitle.language.as_deref(), Some("en"));

    let subtitles = f.subtitles().await;
    assert_eq!(subtitles.len(), own + 1);
    let SubtitleSource::External { path } = &subtitles[own].source else {
        panic!("not a file");
    };
    assert!(path.starts_with(f.downloads.dir()));
    let kept = std::fs::read(path).unwrap();
    assert_eq!(std::str::from_utf8(&kept).unwrap(), RUSSIAN);

    // Bob sees it too, and may not remove it; Alice's again is the same.
    let descriptor = f.prober.describe(&f.media).await.unwrap();
    let seen = f
        .downloads
        .downloads(&film(), &descriptor, &bob().actor)
        .await
        .unwrap();
    assert!(seen[own].as_ref().is_some_and(|d| !d.can_remove));
    let again = f.downloads.download(&film(), 103, &bob()).await.unwrap();
    assert_eq!(again.track, got.track);
    assert_eq!(f.site_calls("/api/v1/download").await, 1);
    let searched = f.downloads.search(&film(), None).await.unwrap();
    assert!(
        searched
            .results
            .iter()
            .any(|r| r.file_id == 103 && r.downloaded)
    );
}

/// One not found by the hash is synced; one found by it is not.
#[tokio::test]
async fn only_a_download_not_matched_by_hash_is_synced() {
    let f = fixture("sync", 0).await;
    f.answers_search().await;
    f.serves(101, RUSSIAN.as_bytes().to_vec(), 5).await;
    f.serves(102, RUSSIAN.as_bytes().to_vec(), 4).await;
    f.downloads.search(&film(), None).await.unwrap();
    let matched = f.downloads.download(&film(), 101, &alice()).await.unwrap();
    let other = f.downloads.download(&film(), 102, &alice()).await.unwrap();
    let descriptor = f.prober.describe(&f.media).await.unwrap();
    let timings = f.timings.timings(&descriptor).await;
    assert_eq!(
        timings[matched.track as usize].as_ref().unwrap().sync,
        SubtitleSync::Original
    );
    // Syncing, or (no ffmpeg in this test) failed: it was synced.
    assert_ne!(
        timings[other.track as usize].as_ref().unwrap().sync,
        SubtitleSync::Original
    );
}

#[tokio::test]
async fn a_result_no_search_offered_is_not_downloaded() {
    let f = fixture("unoffered", 0).await;
    assert!(matches!(
        f.downloads.download(&film(), 101, &alice()).await,
        Err(DownloadError::NotFound(_))
    ));
    assert_eq!(f.site_calls("/api/v1/download").await, 0);
}

/// With a limit of 2, a user's third download that day is refused for
/// them, and another user still may.
#[tokio::test]
async fn a_user_s_daily_limit_holds() {
    let f = fixture("limit", 2).await;
    f.answers_search().await;
    for id in [101, 102, 103, 104] {
        f.serves(id, RUSSIAN.as_bytes().to_vec(), 10).await;
    }
    f.downloads.search(&film(), None).await.unwrap();
    let first = f.downloads.download(&film(), 101, &alice()).await.unwrap();
    assert_eq!(first.user_remaining, Some(1));
    f.downloads.download(&film(), 102, &alice()).await.unwrap();
    let third = f.downloads.download(&film(), 103, &alice()).await;
    assert!(matches!(
        third,
        Err(DownloadError::QuotaExhausted {
            whose: QuotaOf::User,
            resets_at: Some(_)
        })
    ));
    f.downloads.download(&film(), 103, &bob()).await.unwrap();
}

#[tokio::test]
async fn an_exhausted_account_is_said_so() {
    let f = fixture("account", 0).await;
    f.answers_search().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/download"))
        .respond_with(ResponseTemplate::new(406).set_body_json(serde_json::json!({
            "remaining": 0,
            "message": "You have downloaded your allowed 5 subtitles for 24h.",
            "reset_time_utc": "2030-01-01T00:00:00.000Z"
        })))
        .mount(&f.site)
        .await;
    f.downloads.search(&film(), None).await.unwrap();
    assert!(matches!(
        f.downloads.download(&film(), 101, &alice()).await,
        Err(DownloadError::QuotaExhausted {
            whose: QuotaOf::Account,
            resets_at: Some(1_893_456_000)
        })
    ));
}

/// A user an admin took downloading from can search, not download.
#[tokio::test]
async fn a_user_without_the_permission_searches_but_does_not_download() {
    let f = fixture("denied", 0).await;
    f.answers_search().await;
    let principal = alice().actor.principal.to_string();
    f.store
        .set_may_download_subtitles(&principal, false, "local:root")
        .await
        .unwrap();
    f.downloads.search(&film(), None).await.unwrap();
    assert!(matches!(
        f.downloads.download(&film(), 101, &alice()).await,
        Err(DownloadError::NotAllowed(_))
    ));
}

/// Removed by its downloader, its file and track go and its result is
/// listed last, marked; another user may not remove one, an admin may.
#[tokio::test]
async fn a_download_is_removed_by_its_downloader_or_an_admin_only() {
    let f = fixture("remove", 0).await;
    f.answers_search().await;
    f.serves(101, RUSSIAN.as_bytes().to_vec(), 5).await;
    f.serves(102, RUSSIAN.as_bytes().to_vec(), 4).await;
    let own = f.subtitles().await.len();
    f.downloads.search(&film(), None).await.unwrap();
    let first = f.downloads.download(&film(), 101, &alice()).await.unwrap();
    let second = f.downloads.download(&film(), 102, &alice()).await.unwrap();
    let first_id = first.subtitle.download.unwrap().id;
    let second_id = second.subtitle.download.unwrap().id;

    assert!(matches!(
        f.downloads.remove(&film(), &first_id, &bob().actor).await,
        Err(DownloadError::NotAllowed(_))
    ));
    f.downloads
        .remove(&film(), &first_id, &alice().actor)
        .await
        .unwrap();
    // Gone already: done.
    f.downloads
        .remove(&film(), &first_id, &alice().actor)
        .await
        .unwrap();
    let left = f.subtitles().await;
    assert_eq!(left.len(), own + 1);
    assert!(
        std::fs::read_dir(f.downloads.dir()).unwrap().count() == 1,
        "the removed file is gone"
    );
    let searched = f.downloads.search(&film(), None).await.unwrap();
    let last = searched.results.last().unwrap();
    assert_eq!((last.file_id, last.rejected), (101, true));

    f.downloads
        .remove(&film(), &second_id, &root().actor)
        .await
        .unwrap();
    assert_eq!(f.subtitles().await.len(), own);
}

/// A server started again gives each file its downloads, and drops a file
/// a download cut short left.
#[tokio::test]
async fn a_restart_restores_the_tracks() {
    let f = fixture("restore", 0).await;
    f.answers_search().await;
    f.serves(101, RUSSIAN.as_bytes().to_vec(), 5).await;
    let own = f.subtitles().await.len();
    f.downloads.search(&film(), None).await.unwrap();
    f.downloads.download(&film(), 101, &alice()).await.unwrap();
    let stray = f.downloads.dir().join("cut-short.partial");
    std::fs::write(&stray, b"half").unwrap();

    let prober = Arc::new(Prober::new(
        Tool::unconfined(fake_ffprobe(f._tmp.path(), "mkv-forced-subs.json")),
        Duration::from_secs(10),
    ));
    let restarted = SubtitleDownloads::new(
        f.store.clone(),
        f.catalog.clone(),
        prober.clone(),
        f.timings.clone(),
        f.downloads.dir().to_owned(),
        None,
    )
    .unwrap();
    assert_eq!(restarted.restore().await.unwrap(), 1);
    assert_eq!(
        prober.describe(&f.media).await.unwrap().subtitles.len(),
        own + 1
    );
    assert!(!stray.exists());
}

#[tokio::test]
async fn without_a_site_nothing_is_asked() {
    let f = fixture("off", 0).await;
    f.downloads.configure(None);
    assert!(!f.downloads.is_on());
    assert!(matches!(
        f.downloads.search(&film(), None).await,
        Err(DownloadError::NoSource)
    ));
    assert!(matches!(
        f.downloads.download(&film(), 101, &alice()).await,
        Err(DownloadError::NoSource)
    ));
}

/// Without the Internet a search fails, said as such.
#[tokio::test]
async fn an_unreachable_site_fails_the_search_cleanly() {
    let f = fixture("offline", 0).await;
    let mut settings = settings(&f.site, 0);
    settings.account.api_url = crate::opensubtitles::tests::nothing_listens();
    f.downloads.configure(Some(settings));
    assert!(matches!(
        f.downloads.search(&film(), None).await,
        Err(DownloadError::Site(SiteError::Unreachable(_)))
    ));
}

#[test]
fn an_item_s_site_ids_are_read() {
    let ids = ["plex://movie/5d77", "imdb://tt0078748", "tmdb://348"].map(String::from);
    assert_eq!(site_ids(&ids), (Some(78_748), Some(348)));
    assert_eq!(site_ids(&["imdb://ttx".into()]), (None, None));
}
