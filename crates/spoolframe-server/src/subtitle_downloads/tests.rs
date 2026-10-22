//! Searching, downloading and removing against a stand-in site on
//! loopback (wiremock) answering with recorded responses: never the real
//! API.

use spoolframe_media::sandbox::Tool;
use spoolframe_types::{
    MediaKind, Namespace, SubtitleSourceState, SubtitleSourceStatus, SubtitleSync,
};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::principal::Principal;
use crate::store::tests::{TestStore, test_store};
use crate::store::users::LocalUserId;
use crate::subtitle_sync::{FfmpegSpeech, SyncSettings};
use crate::test_support::{FakeCatalog, fake_ffprobe, movie};

const SEARCH: &str = include_str!("../../tests/fixtures/opensubtitles/search.json");
const SUBDL_SEARCH: &str = crate::subdl::tests::SEARCH;

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

/// OpenSubtitles at `site`'s `/api/v1`.
fn opensubtitles_at(site: &MockServer) -> SiteSettings {
    SiteSettings {
        languages: vec!["en".into()],
        kind: SiteKind::OpenSubtitles(opensubtitles::Account {
            api_url: url::Url::parse(&format!("{}/api/v1", site.uri())).unwrap(),
            username: None,
            password: None,
        }),
    }
}

/// SubDL at `site`'s `/subdl/api/v1`, its files at `/subtitle/…`.
fn subdl_at(site: &MockServer) -> SiteSettings {
    SiteSettings {
        languages: vec!["en".into()],
        kind: SiteKind::SubDl(subdl::Account {
            api_url: url::Url::parse(&format!("{}/subdl/api/v1", site.uri())).unwrap(),
            download_url: url::Url::parse(&site.uri()).unwrap(),
            api_key: "subdl-key".into(),
        }),
    }
}

fn settings(site: &MockServer, limit: u32) -> Settings {
    Settings {
        sites: vec![opensubtitles_at(site)],
        daily_limit_per_user: limit,
    }
}

async fn fixture(test: &str, limit: u32) -> Fixture {
    fixture_with(test, |site| settings(site, limit)).await
}

async fn fixture_with(test: &str, settings: impl FnOnce(&MockServer) -> Settings) -> Fixture {
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
        settings(&site),
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

    async fn subdl_answers_search(&self) {
        Mock::given(method("GET"))
            .and(path("/subdl/api/v1/subtitles"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SUBDL_SEARCH))
            .mount(&self.site)
            .await;
    }

    /// Serves SubDL's file at `path` as `bytes`.
    async fn subdl_serves(&self, file: &str, bytes: Vec<u8>) {
        Mock::given(method("GET"))
            .and(path(file))
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
    let found = f
        .downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    assert_eq!(found.languages, ["en"]);
    let ids: Vec<&str> = found.results.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "opensubtitles:101",
            "opensubtitles:102",
            "opensubtitles:103",
            "opensubtitles:104"
        ]
    );
    assert!(found.results.iter().all(|r| r.source == "opensubtitles"));
    assert_eq!(
        found.sources,
        [SubtitleSourceStatus {
            source: "opensubtitles".into(),
            state: SubtitleSourceState::Found { found: 4 },
        }]
    );
    assert!(found.results[0].hash_match);
    assert_eq!(found.results[1].rating, Some(91));
    assert_eq!(found.results[1].format.as_deref(), Some("ass"));
    assert_eq!(found.results[2].origin, SubtitleOrigin::MachineTranslated);
    assert_eq!(found.results[3].origin, SubtitleOrigin::Ai);
    assert_eq!(found.results[0].fps_milli, Some(23_976));
    assert!(found.results[3].forced);
    assert!(!found.results[0].forced);
}

/// Opening the menu twice asks the site once; a day later, again.
#[tokio::test]
async fn a_search_is_kept_a_day() {
    let f = fixture("kept", 0).await;
    f.answers_search().await;
    f.downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    f.downloads
        .search(&film(), Some("EN"), &alice().actor)
        .await
        .unwrap();
    assert_eq!(f.site_calls("/api/v1/subtitles").await, 1);
    f.store.clock().advance(SEARCH_LIFE);
    f.downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
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
    f.downloads
        .search(&film(), Some("en"), &alice().actor)
        .await
        .unwrap();
    f.catalog
        .give_ids(&film(), &["imdb://tt0078748", "tmdb://348"]);
    f.downloads
        .search(&film(), Some("fr"), &alice().actor)
        .await
        .unwrap();
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
    f.downloads
        .search(&episode, None, &alice().actor)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_bad_language_is_refused_before_the_site_is_asked() {
    let f = fixture("language", 0).await;
    assert!(matches!(
        f.downloads
            .search(&film(), Some("en&imdb_id=1"), &alice().actor)
            .await,
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
    f.downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    let got = f
        .downloads
        .download(&film(), "opensubtitles:103", &alice())
        .await
        .unwrap();
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
    let again = f
        .downloads
        .download(&film(), "opensubtitles:103", &bob())
        .await
        .unwrap();
    assert_eq!(again.track, got.track);
    assert_eq!(f.site_calls("/api/v1/download").await, 1);
    let searched = f
        .downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    assert!(
        searched
            .results
            .iter()
            .any(|r| r.id == "opensubtitles:103" && r.downloaded)
    );
}

/// One not found by the hash is synced; one found by it is not.
#[tokio::test]
async fn only_a_download_not_matched_by_hash_is_synced() {
    let f = fixture("sync", 0).await;
    f.answers_search().await;
    f.serves(101, RUSSIAN.as_bytes().to_vec(), 5).await;
    f.serves(102, RUSSIAN.as_bytes().to_vec(), 4).await;
    f.downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    let matched = f
        .downloads
        .download(&film(), "opensubtitles:101", &alice())
        .await
        .unwrap();
    let other = f
        .downloads
        .download(&film(), "opensubtitles:102", &alice())
        .await
        .unwrap();
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
        f.downloads
            .download(&film(), "opensubtitles:101", &alice())
            .await,
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
    f.downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    let first = f
        .downloads
        .download(&film(), "opensubtitles:101", &alice())
        .await
        .unwrap();
    assert_eq!(first.user_remaining, Some(1));
    f.downloads
        .download(&film(), "opensubtitles:102", &alice())
        .await
        .unwrap();
    let third = f
        .downloads
        .download(&film(), "opensubtitles:103", &alice())
        .await;
    assert!(matches!(
        third,
        Err(DownloadError::QuotaExhausted {
            whose: QuotaOf::User,
            resets_at: Some(_),
            site: None,
        })
    ));
    f.downloads
        .download(&film(), "opensubtitles:103", &bob())
        .await
        .unwrap();
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
    f.downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    assert!(matches!(
        f.downloads
            .download(&film(), "opensubtitles:101", &alice())
            .await,
        Err(DownloadError::QuotaExhausted {
            whose: QuotaOf::Account,
            resets_at: Some(1_893_456_000),
            site: Some(Source::OpenSubtitles),
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
    f.downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    assert!(matches!(
        f.downloads
            .download(&film(), "opensubtitles:101", &alice())
            .await,
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
    f.downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    let first = f
        .downloads
        .download(&film(), "opensubtitles:101", &alice())
        .await
        .unwrap();
    let second = f
        .downloads
        .download(&film(), "opensubtitles:102", &alice())
        .await
        .unwrap();
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
    let searched = f
        .downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    let last = searched.results.last().unwrap();
    assert_eq!(
        (last.id.as_str(), last.rejected),
        ("opensubtitles:101", true)
    );

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
    f.downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    f.downloads
        .download(&film(), "opensubtitles:101", &alice())
        .await
        .unwrap();
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
        Settings::default(),
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
    f.downloads.configure(Settings::default());
    assert!(!f.downloads.is_on());
    assert!(matches!(
        f.downloads.search(&film(), None, &alice().actor).await,
        Err(DownloadError::NoSource)
    ));
    assert!(matches!(
        f.downloads
            .download(&film(), "opensubtitles:101", &alice())
            .await,
        Err(DownloadError::NoSource)
    ));
}

/// Without the Internet a search fails, said as such.
#[tokio::test]
async fn an_unreachable_site_fails_the_search_cleanly() {
    let f = fixture("offline", 0).await;
    let mut settings = settings(&f.site, 0);
    let SiteKind::OpenSubtitles(account) = &mut settings.sites[0].kind else {
        unreachable!()
    };
    account.api_url = crate::opensubtitles::tests::nothing_listens();
    f.downloads.configure(settings);
    assert!(matches!(
        f.downloads.search(&film(), None, &alice().actor).await,
        Err(DownloadError::Site {
            site: Source::OpenSubtitles,
            error: SiteError::Unreachable(_)
        })
    ));
}

/// A result for [`order`]: human-made, unrejected, rated `rating`.
fn ranked(id: &str, hash_match: bool, sdh: bool, forced: bool, rating: u8) -> SubtitleResult {
    SubtitleResult {
        id: id.into(),
        source: "opensubtitles".into(),
        release: String::new(),
        language: "en".into(),
        format: None,
        fps_milli: None,
        downloads: 0,
        rating: Some(rating),
        hearing_impaired: sdh,
        forced,
        origin: SubtitleOrigin::Human,
        hash_match,
        rejected: false,
        downloaded: false,
    }
}

/// `matched` (found by the hash, SDH), `sdh` (rated 90), `plain` (rated
/// 10) and `forced` (rated 95), ordered for `sdh` and `forced`
/// preferences: their ids.
fn ordered(sdh: SdhPreference, forced: ForcedPreference) -> Vec<String> {
    let mut results = vec![
        ranked("sdh", false, true, false, 90),
        ranked("plain", false, false, false, 10),
        ranked("matched", true, true, false, 0),
        ranked("forced", false, false, true, 95),
    ];
    let wanted = SubtitlePreferences {
        sdh,
        forced,
        ..SubtitlePreferences::default()
    };
    order(&mut results, &wanted);
    results.into_iter().map(|r| r.id).collect()
}

/// The defaults: after the hash match, the whole dialogue first, plain
/// before SDH, and forced ones last, whatever their rating.
#[test]
fn by_default_plain_comes_before_sdh_and_forced_after_both() {
    assert_eq!(
        ordered(SdhPreference::Accept, ForcedPreference::Accept),
        ["matched", "plain", "sdh", "forced"]
    );
}

#[test]
fn sdh_preferred_comes_first_after_the_hash_match() {
    assert_eq!(
        ordered(SdhPreference::Prefer, ForcedPreference::Accept),
        ["matched", "sdh", "plain", "forced"]
    );
}

/// "Only with" and "only without" filter: even a hash match goes.
#[test]
fn sdh_avoided_or_only_filters_the_results() {
    assert_eq!(
        ordered(SdhPreference::Avoid, ForcedPreference::Accept),
        ["plain", "forced"]
    );
    assert_eq!(
        ordered(SdhPreference::Only, ForcedPreference::Accept),
        ["matched", "sdh"]
    );
}

#[test]
fn forced_preferred_comes_first_after_the_hash_match() {
    assert_eq!(
        ordered(SdhPreference::Accept, ForcedPreference::Prefer),
        ["matched", "forced", "plain", "sdh"]
    );
}

#[test]
fn forced_avoided_or_only_filters_the_results() {
    assert_eq!(
        ordered(SdhPreference::Accept, ForcedPreference::Avoid),
        ["matched", "plain", "sdh"]
    );
    assert_eq!(
        ordered(SdhPreference::Accept, ForcedPreference::Only),
        ["forced"]
    );
}

/// The asker's own preferences order their search: Alice, who wants no
/// SDH, is not offered OpenSubtitles' SDH result; Bob, with the
/// defaults, is.
#[tokio::test]
async fn a_search_follows_the_asker_s_preferences() {
    let f = fixture("preferences", 0).await;
    f.answers_search().await;
    let mut no_sdh = spoolframe_types::TrackPreferences::default();
    no_sdh.subtitles.sdh = SdhPreference::Avoid;
    f.store
        .set_track_preferences(alice().actor.principal, &no_sdh)
        .await
        .unwrap();
    let ids = |found: SubtitleSearch| -> Vec<String> {
        found.results.into_iter().map(|r| r.id).collect()
    };
    let alice_s = f
        .downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    assert!(!ids(alice_s).contains(&"opensubtitles:102".to_owned()));
    let bob_s = f
        .downloads
        .search(&film(), None, &bob().actor)
        .await
        .unwrap();
    assert!(ids(bob_s).contains(&"opensubtitles:102".to_owned()));
}

#[test]
fn an_item_s_site_ids_are_read() {
    let ids = ["plex://movie/5d77", "imdb://tt0078748", "tmdb://348"].map(String::from);
    assert_eq!(site_ids(&ids), (Some(78_748), Some(348)));
    assert_eq!(site_ids(&["imdb://ttx".into()]), (None, None));
}

/// A zip of `entries` (name, bytes), as SubDL sends a subtitle.
fn zipped(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn both(site: &MockServer) -> Settings {
    Settings {
        sites: vec![opensubtitles_at(site), subdl_at(site)],
        daily_limit_per_user: 0,
    }
}

/// With both on, one search asks both and lists what both found, each
/// named by its site: the hash match first, then the plain ones,
/// OpenSubtitles' rated and downloaded ones before SubDL's, which gives
/// neither, in its own order; OpenSubtitles' SDH one after them, as a
/// user with the default SDH preference wants.
#[tokio::test]
async fn both_sites_are_searched_and_their_results_are_one_list() {
    let f = fixture_with("both", both).await;
    f.answers_search().await;
    f.subdl_answers_search().await;
    let found = f
        .downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    let ids: Vec<&str> = found.results.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "opensubtitles:101",
            "subdl:3172974-3188291",
            "subdl:2978466-2972819",
            "subdl:504692-1185",
            "opensubtitles:102",
            "opensubtitles:103",
            "opensubtitles:104",
        ]
    );
    let subdl = &found.results[1];
    assert_eq!(subdl.source, "subdl");
    // SubDL says nothing of foreign parts: none of its is forced.
    assert_eq!(
        (
            subdl.downloads,
            subdl.rating,
            subdl.hash_match,
            subdl.forced
        ),
        (0, None, false, false)
    );
    assert_eq!(subdl.fps_milli, Some(23_976));
    assert_eq!(
        found.sources,
        [
            SubtitleSourceStatus {
                source: "opensubtitles".into(),
                state: SubtitleSourceState::Found { found: 4 },
            },
            SubtitleSourceStatus {
                source: "subdl".into(),
                state: SubtitleSourceState::Found { found: 3 },
            },
        ]
    );
    // Each site was asked once, SubDL with its key and the film's title
    // (it has no id), in SubDL's code.
    assert_eq!(f.site_calls("/api/v1/subtitles").await, 1);
    let asked = f.site.received_requests().await.unwrap();
    let subdl_asked = asked
        .iter()
        .find(|r| r.url.path() == "/subdl/api/v1/subtitles")
        .unwrap();
    let pairs: HashMap<_, _> = subdl_asked.url.query_pairs().into_owned().collect();
    assert_eq!(pairs["api_key"], "subdl-key");
    assert_eq!(pairs["film_name"], "Alien");
    assert_eq!(pairs["type"], "movie");
    assert_eq!(pairs["languages"], "EN");
    // Kept a day, as OpenSubtitles' are.
    f.downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    assert_eq!(f.site.received_requests().await.unwrap().len(), 2);
}

/// One site down or out of quota leaves the other's results, and says
/// so; both down, the search fails with the first one's error.
#[tokio::test]
async fn one_site_failing_leaves_the_other_s_results() {
    let f = fixture_with("one-down", both).await;
    f.answers_search().await;
    Mock::given(method("GET"))
        .and(path("/subdl/api/v1/subtitles"))
        .and(query_param("languages", "EN"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&f.site)
        .await;
    Mock::given(method("GET"))
        .and(path("/subdl/api/v1/subtitles"))
        .and(query_param("languages", "FR"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "status": false, "error": "Daily request limit exceeded",
        })))
        .mount(&f.site)
        .await;
    let found = f
        .downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    assert_eq!(found.results.len(), 4);
    assert!(found.results.iter().all(|r| r.source == "opensubtitles"));
    assert!(matches!(
        &found.sources[1],
        SubtitleSourceStatus { source, state: SubtitleSourceState::Failed { message } }
            if source == "subdl" && message.starts_with("SubDL cannot be reached")
    ));
    let french = f
        .downloads
        .search(&film(), Some("fr"), &alice().actor)
        .await
        .unwrap();
    assert_eq!(
        french.sources[1].state,
        SubtitleSourceState::QuotaExhausted { resets_at: None }
    );
    // A failure is not kept: the next search asks SubDL again.
    f.downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    let subdl_calls = f
        .site
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/subdl/api/v1/subtitles")
        .count();
    assert_eq!(subdl_calls, 3);

    // Both down: the first site's error.
    let mut settings = both(&f.site);
    let SiteKind::OpenSubtitles(account) = &mut settings.sites[0].kind else {
        unreachable!()
    };
    account.api_url = crate::opensubtitles::tests::nothing_listens();
    f.downloads.configure(settings);
    assert!(matches!(
        f.downloads
            .search(&film(), Some("de"), &alice().actor)
            .await,
        Err(DownloadError::Site {
            site: Source::OpenSubtitles,
            error: SiteError::Unreachable(_)
        })
    ));
}

/// A SubDL result is downloaded from SubDL: its zip opened, the CP-1251
/// SRT in it kept as UTF-8, a track titled "SubDL" at once, and synced,
/// as nothing SubDL has is matched by the file's hash.
#[tokio::test]
async fn a_subdl_download_comes_out_of_its_archive_and_is_synced() {
    let f = fixture_with("subdl", both).await;
    f.answers_search().await;
    f.subdl_answers_search().await;
    let (cp1251, _, _) = encoding_rs::WINDOWS_1251.encode(RUSSIAN);
    f.subdl_serves(
        "/subtitle/3172974-3188291.zip",
        zipped(&[("Film.nfo", b"made by someone"), ("Film.2019.srt", &cp1251)]),
    )
    .await;
    let own = f.subtitles().await.len();
    f.downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    let got = f
        .downloads
        .download(&film(), "subdl:3172974-3188291", &alice())
        .await
        .unwrap();
    assert_eq!(got.track as usize, own);
    assert_eq!(got.account_remaining, None);
    let download = got.subtitle.download.clone().unwrap();
    assert_eq!(download.source, "subdl");
    assert_eq!(download.release, "Film.2019.1080p.BluRay.x264-GRP");
    assert_eq!(got.subtitle.title.as_deref(), Some("SubDL"));
    let subtitles = f.subtitles().await;
    let SubtitleSource::External { path } = &subtitles[own].source else {
        panic!("not a file");
    };
    assert_eq!(
        std::str::from_utf8(&std::fs::read(path).unwrap()).unwrap(),
        RUSSIAN
    );
    let descriptor = f.prober.describe(&f.media).await.unwrap();
    let timings = f.timings.timings(&descriptor).await;
    assert_ne!(
        timings[got.track as usize].as_ref().unwrap().sync,
        SubtitleSync::Original
    );
    // The key went in a header; the file's URL has no query.
    let fetched = f
        .site
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.url.path() == "/subtitle/3172974-3188291.zip")
        .unwrap();
    assert_eq!(fetched.url.query(), None);
    assert_eq!(
        fetched.headers.get("x-api-key").unwrap().to_str().unwrap(),
        "subdl-key"
    );
    // Searched again, it is marked downloaded; the same id from the
    // other site is another result.
    let again = f
        .downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    assert!(
        again
            .results
            .iter()
            .any(|r| r.id == "subdl:3172974-3188291" && r.downloaded)
    );
    assert!(
        !again
            .results
            .iter()
            .any(|r| r.source == "opensubtitles" && r.downloaded)
    );
}

/// An archive that climbs out of itself, or holds no subtitle, is not
/// kept, and nothing of it is written.
#[tokio::test]
async fn a_bad_subdl_archive_is_refused() {
    let f = fixture_with("subdl-bad", both).await;
    f.answers_search().await;
    f.subdl_answers_search().await;
    f.subdl_serves(
        "/subtitle/3172974-3188291.zip",
        zipped(&[("../../../etc/cron.d/x.srt", RUSSIAN.as_bytes())]),
    )
    .await;
    f.subdl_serves(
        "/subtitle/2978466-2972819.zip",
        zipped(&[("Film.sub", b"{1}{2}Hello")]),
    )
    .await;
    f.downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    for result in ["subdl:3172974-3188291", "subdl:2978466-2972819"] {
        assert!(
            matches!(
                f.downloads.download(&film(), result, &alice()).await,
                Err(DownloadError::InvalidInput(_))
            ),
            "{result}"
        );
    }
    assert_eq!(std::fs::read_dir(f.downloads.dir()).unwrap().count(), 0);
    assert!(
        f.store
            .subtitle_downloads(&film().to_string())
            .await
            .unwrap()
            .is_empty()
    );
}

/// SubDL alone: OpenSubtitles is never asked, nor the file hashed for it.
/// A result's id names its site: one of a site that is off, or of none,
/// is not downloaded.
#[tokio::test]
async fn subdl_alone_asks_only_subdl() {
    let f = fixture_with("subdl-alone", |site| Settings {
        sites: vec![subdl_at(site)],
        daily_limit_per_user: 0,
    })
    .await;
    f.subdl_answers_search().await;
    let found = f
        .downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    assert_eq!(found.results.len(), 3);
    assert_eq!(f.site_calls("/api/v1/subtitles").await, 0);
    assert!(matches!(
        f.downloads
            .download(&film(), "opensubtitles:101", &alice())
            .await,
        Err(DownloadError::NotFound(_))
    ));
    for bad in ["101", "podnapisi:1", "subdl:"] {
        assert!(
            matches!(
                f.downloads.download(&film(), bad, &alice()).await,
                Err(DownloadError::InvalidInput(_))
            ),
            "{bad}"
        );
    }
}

/// An episode is asked of SubDL by its show's id, as SubDL lists a
/// show's subtitles, with its numbers; of a season's pack, the episode's
/// file is the one kept.
#[tokio::test]
async fn an_episode_is_asked_of_subdl_by_its_show_and_taken_from_a_pack() {
    let f = fixture_with("subdl-episode", |site| Settings {
        sites: vec![subdl_at(site)],
        daily_limit_per_user: 0,
    })
    .await;
    let show = MediaId::new(Namespace::Plex, MediaKind::Show, "10").unwrap();
    let episode = MediaId::new(Namespace::Plex, MediaKind::Episode, "15").unwrap();
    crate::test_support::write_episode(&f.media, "15");
    f.catalog.give_ids(&show, &["imdb://tt2193021"]);
    Mock::given(method("GET"))
        .and(path("/subdl/api/v1/subtitles"))
        .and(query_param("imdb_id", "tt2193021"))
        .and(query_param("type", "tv"))
        .and(query_param("season_number", "2"))
        .and(query_param("episode_number", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SUBDL_SEARCH))
        .expect(1)
        .mount(&f.site)
        .await;
    let other = "1\r\n00:00:01,000 --> 00:00:02,000\r\nWrong episode\r\n";
    f.subdl_serves(
        "/subtitle/3172974-3188291.zip",
        zipped(&[
            ("Show.S02E02.srt", [other, other, other].concat().as_bytes()),
            ("Show.S02E01.srt", RUSSIAN.as_bytes()),
        ]),
    )
    .await;
    f.downloads
        .search(&episode, None, &alice().actor)
        .await
        .unwrap();
    f.downloads
        .download(&episode, "subdl:3172974-3188291", &alice())
        .await
        .unwrap();
    let kept: Vec<_> = std::fs::read_dir(f.downloads.dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(std::fs::read_to_string(&kept[0]).unwrap(), RUSSIAN);
}

/// Turned off, live (out of the sources, or every site gone), a site's
/// downloads stay: the tracks, their source, and who may remove them.
#[tokio::test]
async fn downloads_stay_when_their_site_is_turned_off() {
    let f = fixture_with("kept", both).await;
    f.answers_search().await;
    f.subdl_answers_search().await;
    f.serves(101, RUSSIAN.as_bytes().to_vec(), 5).await;
    f.subdl_serves(
        "/subtitle/3172974-3188291.zip",
        zipped(&[("Film.srt", RUSSIAN.as_bytes())]),
    )
    .await;
    let own = f.subtitles().await.len();
    f.downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    f.downloads
        .download(&film(), "opensubtitles:101", &alice())
        .await
        .unwrap();
    let subdl = f
        .downloads
        .download(&film(), "subdl:3172974-3188291", &alice())
        .await
        .unwrap();
    f.downloads.configure(Settings {
        sites: vec![opensubtitles_at(&f.site)],
        daily_limit_per_user: 0,
    });
    f.downloads.configure(Settings::default());

    assert_eq!(f.subtitles().await.len(), own + 2);
    let descriptor = f.prober.describe(&f.media).await.unwrap();
    let seen = f
        .downloads
        .downloads(&film(), &descriptor, &alice().actor)
        .await
        .unwrap();
    let sources: Vec<(&str, bool)> = seen[own..]
        .iter()
        .map(|d| {
            d.as_ref()
                .map(|d| (d.source.as_str(), d.can_remove))
                .unwrap()
        })
        .collect();
    assert_eq!(sources, [("opensubtitles", true), ("subdl", true)]);
    let id = subdl.subtitle.download.unwrap().id;
    f.downloads
        .remove(&film(), &id, &alice().actor)
        .await
        .unwrap();
    assert_eq!(f.subtitles().await.len(), own + 1);
}

/// A site's part of a search that failed says only how, as a failed
/// download does: what the site said stays in the log.
#[tokio::test]
async fn a_failed_site_s_own_words_are_not_sent_to_users() {
    let f = fixture_with("redacted", both).await;
    f.answers_search().await;
    Mock::given(method("GET"))
        .and(path("/subdl/api/v1/subtitles"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "status": false, "error": "key of account 48151623 is suspended",
        })))
        .mount(&f.site)
        .await;
    let found = f
        .downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    assert_eq!(
        found.sources[1].state,
        SubtitleSourceState::Failed {
            message: "SubDL refused the server's request".into()
        }
    );
}

/// One site's slow download does not hold up the other's: the quick one
/// ends while the slow one is still waiting for SubDL. Nothing is timed
/// against the wall clock, so a loaded machine can't fail it; SubDL's
/// answer is held long enough that it can only come after.
#[tokio::test]
async fn a_slow_download_from_one_site_does_not_hold_up_the_other() {
    let f = fixture_with("apart", both).await;
    f.answers_search().await;
    f.subdl_answers_search().await;
    f.serves(101, RUSSIAN.as_bytes().to_vec(), 5).await;
    Mock::given(method("GET"))
        .and(path("/subtitle/3172974-3188291.zip"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(zipped(&[("Film.srt", RUSSIAN.as_bytes())]))
                .set_delay(Duration::from_secs(120)),
        )
        .mount(&f.site)
        .await;
    f.downloads
        .search(&film(), None, &alice().actor)
        .await
        .unwrap();
    let (id, alice) = (film(), alice());
    let slow = f.downloads.download(&id, "subdl:3172974-3188291", &alice);
    tokio::pin!(slow);
    // Bob asks only once Alice's download is at SubDL, waiting on it.
    let at_subdl = async {
        loop {
            let asked = f.site.received_requests().await.unwrap_or_default();
            if asked
                .iter()
                .any(|r| r.url.path() == "/subtitle/3172974-3188291.zip")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::select! {
        done = &mut slow => panic!("SubDL's held answer came first: {done:?}"),
        () = at_subdl => {}
        () = tokio::time::sleep(Duration::from_secs(60)) => panic!("never reached SubDL"),
    }
    let bob = bob();
    let quick = f.downloads.download(&id, "opensubtitles:101", &bob);
    tokio::select! {
        biased;
        done = &mut slow => panic!("SubDL's held answer came first: {done:?}"),
        quick = quick => {
            quick.unwrap();
        }
    }
}

/// An episode with ids of its own, searched on OpenSubtitles alone, does
/// not need its show's: they are not asked for. SubDL, which lists a
/// show's subtitles, does need them.
#[tokio::test]
async fn a_show_s_ids_are_asked_for_only_when_a_site_needs_them() {
    let show = MediaId::new(Namespace::Plex, MediaKind::Show, "10").unwrap();
    let episode = MediaId::new(Namespace::Plex, MediaKind::Episode, "15").unwrap();
    let f = fixture("show-ids", 0).await;
    crate::test_support::write_episode(&f.media, "15");
    f.catalog.give_ids(&episode, &["imdb://tt2193022"]);
    Mock::given(method("GET"))
        .and(path("/api/v1/subtitles"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
        .mount(&f.site)
        .await;
    f.catalog.take_calls(&show);
    f.downloads
        .search(&episode, None, &alice().actor)
        .await
        .unwrap();
    assert!(
        !f.catalog.take_calls(&show).contains(&"external_ids"),
        "OpenSubtitles alone, by the episode's own id"
    );
    f.downloads.configure(Settings {
        sites: vec![subdl_at(&f.site)],
        daily_limit_per_user: 0,
    });
    Mock::given(method("GET"))
        .and(path("/subdl/api/v1/subtitles"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": false, "error": "can't find movie or tv",
        })))
        .mount(&f.site)
        .await;
    f.downloads
        .search(&episode, None, &alice().actor)
        .await
        .unwrap();
    assert!(f.catalog.take_calls(&show).contains(&"external_ids"));
}
