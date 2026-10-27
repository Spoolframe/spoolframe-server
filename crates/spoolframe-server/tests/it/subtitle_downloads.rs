//! Subtitles from OpenSubtitles and SubDL on demand (PX-02) over a real Iroh
//! connection and the admin API, with real ffmpeg and ffprobe on a film,
//! against a stand-in site on loopback answering as the real one does
//! (never the real API): a search, a download that is every user's track
//! at once and reads as UTF-8, who may remove it, the quotas, and a client
//! without the feature sent none of it; both sites at once. The parts are
//! tested in `subtitle_downloads::tests`, `opensubtitles::tests` and
//! `subdl::tests`.

use crate::support;

use spoolframe_admin_api::{
    AdminApiClient, SubtitleAccountCount, SubtitleSiteUsage, SubtitleUserUsage, error_code,
};
use spoolframe_media::subsync::{self, Span, TextFormat};
use spoolframe_protocol::{
    BitmapSubtitles, Error, Hello, PROTOCOL_VERSION, PlaybackHandle, Reply, Request, Response,
    Selection, SessionId,
};
use spoolframe_server::opensubtitles::Account;
use spoolframe_server::subtitle_downloads::{Settings, SiteKind, SiteSettings};
use spoolframe_types::{
    AudioCodec, ClientCapabilities, Container, MediaTracks, QuotaOf, SubtitleDownloaded,
    SubtitleFormat, SubtitleSearch, VideoCodec,
};
use support::{ServerOptions, TestClient, TestServer, fresh_dir, movie};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::admin::code;
use crate::playback_debug::admin_beside;

const SEARCH: &str = include_str!("../fixtures/opensubtitles/search.json");

/// An SRT in CP-1251.
const RUSSIAN: &str = "1\r\n00:00:01,000 --> 00:00:02,500\r\nПривет, мир\r\n\r\n\
                       2\r\n00:00:03,000 --> 00:00:04,000\r\nКак дела?\r\n";

/// Twenty lines, where the film's speech has them (a stand-in finds it).
fn cues() -> Vec<Span> {
    (0..20)
        .map(|i| Span {
            start_ms: 1_000 + i * 400 + (i % 3) * 90,
            end_ms: 1_250 + i * 400 + (i % 3) * 90 + (i % 4) * 40,
        })
        .collect()
}

fn srt(cues: &[Span], shift_ms: i64) -> String {
    cues.iter()
        .enumerate()
        .map(|(i, c)| {
            let time = |ms: i64| format!("00:00:{:02},{:03}", ms / 1000, ms % 1000);
            format!(
                "{}\n{} --> {}\nLine {i}\n\n",
                i + 1,
                time(c.start_ms + shift_ms),
                time(c.end_ms + shift_ms)
            )
        })
        .collect()
}

/// A stand-in site answering the recorded search, and serving every file
/// as the CP-1251 SRT.
pub(crate) async fn site() -> MockServer {
    site_with_remaining(17).await
}

/// [`site`], whose account has `remaining` downloads left after each.
pub(crate) async fn site_with_remaining(remaining: u32) -> MockServer {
    site_serving(remaining, 200).await
}

/// How many downloads `site` was asked to grant.
pub(crate) async fn grants_asked(site: &MockServer) -> usize {
    site.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST && r.url.path() == "/api/v1/download")
        .count()
}

/// [`site_with_remaining`], whose files are answered `file_status`.
pub(crate) async fn site_serving(remaining: u32, file_status: u16) -> MockServer {
    let site = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/subtitles"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SEARCH))
        .mount(&site)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "link": format!("{}/files/one.srt", site.uri()),
            "remaining": remaining,
            "reset_time_utc": "2030-01-01T00:00:00.000Z"
        })))
        .mount(&site)
        .await;
    let (cp1251, _, _) = encoding_rs::WINDOWS_1251.encode(RUSSIAN);
    Mock::given(method("GET"))
        .and(path("/files/one.srt"))
        .respond_with(ResponseTemplate::new(file_status).set_body_bytes(cp1251.into_owned()))
        .mount(&site)
        .await;
    site
}

pub(crate) fn settings(site: &MockServer, limit: u32) -> Settings {
    Settings {
        sites: vec![SiteSettings {
            languages: vec!["en".into()],
            kind: SiteKind::OpenSubtitles(Account {
                api_url: url::Url::parse(&format!("{}/api/v1", site.uri())).unwrap(),
                username: None,
                password: None,
            }),
        }],
        daily_limit_per_user: limit,
        daily_limit_per_peer: limit,
    }
}

async fn ready(test: &str, site: Settings) -> TestServer {
    let server = film(test, site).await;
    server
        .add_user("alice", "correct horse", Some("Alice"))
        .await;
    server.add_user("bob", "correct horse", Some("Bob")).await;
    server
}

/// The server with its film, and no user yet.
async fn film(test: &str, site: Settings) -> TestServer {
    film_in(fresh_dir(test).await, site).await
}

async fn film_in(dir: support::StateDir, site: Settings) -> TestServer {
    let server = TestServer::start_with(
        dir,
        ServerOptions {
            real_media: true,
            subtitle_sites: site,
            speech: Some(cues()),
            ..ServerOptions::default()
        },
    )
    .await;
    server.generate_media(10, "libx264", "aac");
    server
}

/// A Linux app that plays SubRip and Matroska, directly or converted.
pub(crate) async fn app(server: &TestServer, user: &str) -> (TestClient, SessionId) {
    let capabilities = ClientCapabilities {
        video: [VideoCodec::H264].into(),
        audio: [AudioCodec::Aac].into(),
        containers: [Container::Matroska].into(),
        subtitles: [SubtitleFormat::Srt].into(),
        delivery: vec![
            spoolframe_types::Delivery::Range,
            spoolframe_types::Delivery::Progressive,
        ],
        ..ClientCapabilities::default()
    };
    app_with(server, user, capabilities).await
}

/// An app with `capabilities`.
async fn app_with(
    server: &TestServer,
    user: &str,
    capabilities: ClientCapabilities,
) -> (TestClient, SessionId) {
    let client = server.client().await;
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "test".into(),
        client_version: "0".into(),
        platform: spoolframe_types::Platform::Linux,
        device_name: None,
        capabilities: Some(capabilities),
        experimental: Vec::new(),
    };
    assert!(matches!(
        client.call(None, Request::Hello(hello)).await,
        Response::Ok(Reply::ServerInfo(_))
    ));
    let session = client.login(user, "correct horse").await;
    (client, session)
}

async fn search(client: &TestClient, session: &SessionId) -> Result<SubtitleSearch, Error> {
    match client
        .call(
            Some(session),
            Request::SearchSubtitles {
                id: movie().id().clone(),
                language: None,
            },
        )
        .await
    {
        Response::Ok(Reply::SubtitleResults(found)) => Ok(found),
        Response::Err(e) => Err(e),
        other => panic!("{other:?}"),
    }
}

async fn download(
    client: &TestClient,
    session: &SessionId,
    result: &str,
) -> Result<SubtitleDownloaded, Error> {
    match client
        .call(
            Some(session),
            Request::DownloadSubtitle {
                id: movie().id().clone(),
                result: result.to_owned(),
            },
        )
        .await
    {
        Response::Ok(Reply::SubtitleDownloaded(got)) => Ok(got),
        Response::Err(e) => Err(e),
        other => panic!("{other:?}"),
    }
}

async fn remove(client: &TestClient, session: &SessionId, download: &str) -> Response {
    client
        .call(
            Some(session),
            Request::RemoveSubtitleDownload {
                id: movie().id().clone(),
                download: download.into(),
            },
        )
        .await
}

async fn tracks(client: &TestClient, session: &SessionId) -> MediaTracks {
    match client
        .call(
            Some(session),
            Request::Tracks {
                id: movie().id().clone(),
            },
        )
        .await
    {
        Response::Ok(Reply::Tracks(tracks)) => tracks,
        other => panic!("no tracks: {other:?}"),
    }
}

async fn play(client: &TestClient, session: &SessionId, subtitle: u32) -> PlaybackHandle {
    match client
        .call(
            Some(session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection {
                    audio: Some(0),
                    subtitle: Some(subtitle),
                }),
                start_ms: None,
                bitmap: BitmapSubtitles::Auto,
                quality: Default::default(),
                display: None,
                capabilities: None,
                onward_share: None,
            },
        )
        .await
    {
        Response::Ok(Reply::PlaybackStarted { handle, .. }) => handle,
        other => panic!("playback did not start: {other:?}"),
    }
}

/// Alice finds and downloads one; it is Bob's track at once, plays for
/// him as UTF-8, and only Alice (or an admin) may remove it. Removed, it
/// is gone for both and listed last in the next search, marked.
#[tokio::test]
async fn a_download_is_everyone_s_track_and_its_downloader_s_to_remove() {
    let site = site().await;
    let server = ready("subdl-flow", settings(&site, 0)).await;
    let (alice, alice_session) = app(&server, "alice").await;
    let (bob, bob_session) = app(&server, "bob").await;

    let found = search(&alice, &alice_session).await.unwrap();
    assert_eq!(found.results[0].id, "opensubtitles:101");
    assert!(found.results[0].hash_match);
    let got = download(&alice, &alice_session, "opensubtitles:101")
        .await
        .unwrap();
    assert_eq!(got.account_remaining, Some(17));
    let download_id = got.subtitle.download.clone().unwrap().id;

    let seen = tracks(&bob, &bob_session).await;
    let track = &seen.subtitles[got.track as usize];
    let by_bob = track.download.clone().expect("marked as downloaded");
    assert_eq!((by_bob.by.as_str(), by_bob.can_remove), ("Alice", false));
    assert!(track.external);

    let handle = play(&bob, &bob_session, got.track).await;
    match bob
        .call(
            Some(&bob_session),
            Request::ReadSubtitle {
                handle,
                offset: 0,
                version: None,
            },
        )
        .await
    {
        Response::Ok(Reply::SubtitleChunk { bytes, .. }) => {
            assert_eq!(String::from_utf8(bytes).unwrap(), RUSSIAN);
        }
        other => panic!("{other:?}"),
    }

    assert_eq!(
        remove(&bob, &bob_session, &download_id).await,
        Response::Err(Error::NotAllowed)
    );
    assert_eq!(
        remove(&alice, &alice_session, &download_id).await,
        Response::Ok(Reply::Done)
    );
    assert!(tracks(&bob, &bob_session).await.subtitles.is_empty());
    let again = search(&bob, &bob_session).await.unwrap();
    let last = again.results.last().unwrap();
    assert_eq!(
        (last.id.as_str(), last.rejected),
        ("opensubtitles:101", true)
    );
    server.stop().await;
}

/// With a daily limit of 1 a user's second download is refused for them;
/// an exhausted account is refused for the account.
#[tokio::test]
async fn the_quotas_are_said_as_whose_they_are() {
    let site = site().await;
    let server = ready("subdl-quota", settings(&site, 1)).await;
    let (alice, session) = app(&server, "alice").await;
    search(&alice, &session).await.unwrap();
    let first = download(&alice, &session, "opensubtitles:101")
        .await
        .unwrap();
    assert_eq!(first.user_remaining, Some(0));
    assert!(matches!(
        download(&alice, &session, "opensubtitles:102").await,
        Err(Error::QuotaExhausted {
            whose: QuotaOf::User,
            resets_at: Some(_),
            site: None,
        })
    ));

    site.reset().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/download"))
        .respond_with(ResponseTemplate::new(406).set_body_json(serde_json::json!({
            "remaining": 0,
            "message": "You have downloaded your allowed 5 subtitles for 24h.",
            "reset_time_utc": "2030-01-01T00:00:00.000Z"
        })))
        .mount(&site)
        .await;
    let (bob, bob_session) = app(&server, "bob").await;
    assert_eq!(
        download(&bob, &bob_session, "opensubtitles:102").await,
        Err(Error::QuotaExhausted {
            whose: QuotaOf::Account,
            resets_at: Some(1_893_456_000),
            site: Some("opensubtitles".into()),
        })
    );
    server.stop().await;
}

/// Subtitle search is base protocol: an app that names no feature
/// searches, and is sent the `download` of a track another user
/// downloaded.
#[tokio::test]
async fn an_app_naming_no_feature_searches_and_sees_a_download() {
    let site = site().await;
    let server = ready("subdl-base", settings(&site, 0)).await;
    let (alice, alice_session) = app(&server, "alice").await;
    search(&alice, &alice_session).await.unwrap();
    download(&alice, &alice_session, "opensubtitles:102")
        .await
        .unwrap();
    let (bob, bob_session) = app(&server, "bob").await;
    search(&bob, &bob_session).await.unwrap();
    let seen = tracks(&bob, &bob_session).await;
    assert_eq!(seen.subtitles.len(), 1);
    assert!(seen.subtitles[0].download.is_some());
    assert!(seen.subtitles[0].external);
    server.stop().await;
}

#[tokio::test]
async fn a_server_without_opensubtitles_says_so() {
    let server = ready("subdl-off", Settings::default()).await;
    let (alice, session) = app(&server, "alice").await;
    assert_eq!(search(&alice, &session).await, Err(Error::NoSubtitleSource));
    server.stop().await;
}

/// The web UI's way (rollout stage 1): the admin API searches, downloads
/// and removes, and an admin takes downloading from a user, who can still
/// search.
#[tokio::test]
async fn the_admin_api_searches_downloads_and_turns_downloads_off() {
    let site = site().await;
    let server = film("subdl-admin", settings(&site, 0)).await;
    let (root, _alice) = admin_beside(&server).await;
    server.add_user("bob", "correct horse", Some("Bob")).await;
    let id = movie().id().to_string();
    let found = root.subtitle_search(id.clone(), None).await.unwrap();
    assert_eq!(found.results.len(), 4);
    let got = root
        .subtitle_download(id.clone(), "opensubtitles:103".into())
        .await
        .unwrap();
    let timings = root.subtitle_timings(id.clone()).await.unwrap();
    let row = timings
        .iter()
        .find(|t| t.track == got.track)
        .expect("listed with the subtitle files");
    let downloaded = row.download.clone().unwrap();
    assert!(downloaded.can_remove);
    assert!(row.label.starts_with("OpenSubtitles · "), "{}", row.label);
    root.subtitle_remove(id.clone(), downloaded.id.clone())
        .await
        .unwrap();
    assert!(root.subtitle_timings(id.clone()).await.unwrap().is_empty());
    // The day's downloads: the removed one still counts, against the
    // site's account (as the site said it) and its downloader.
    let usage = root.subtitle_usage().await.unwrap();
    assert_eq!(
        usage.sites,
        [SubtitleSiteUsage {
            source: "opensubtitles".into(),
            downloads: 1,
            account: Some(SubtitleAccountCount {
                used: None,
                remaining: 17,
                limit: None,
                resets_at: Some(1_893_456_000),
                at: usage.sites[0].account.as_ref().unwrap().at,
            }),
        }]
    );
    assert_eq!(usage.daily_limit_per_user, 0);
    assert_eq!(
        usage.users,
        [SubtitleUserUsage {
            username: Some("root".into()),
            name: "root".into(),
            downloads: 1,
            remaining: None,
        }]
    );

    assert!(root.user_subtitle_downloads("bob".into()).await.unwrap());
    root.user_set_subtitle_downloads("bob".into(), false)
        .await
        .unwrap();
    assert!(!root.user_subtitle_downloads("bob".into()).await.unwrap());
    let (bob, session) = app(&server, "bob").await;
    search(&bob, &session).await.unwrap();
    assert_eq!(
        download(&bob, &session, "opensubtitles:101").await,
        Err(Error::NotAllowed)
    );
    assert_eq!(
        code(
            root.subtitle_download(id, "opensubtitles:999".into())
                .await
                .unwrap_err()
        ),
        error_code::INVALID_INPUT
    );
    server.stop().await;
}

/// A stand-in site whose every file is the film's lines 1.5 s late.
async fn late_site() -> MockServer {
    let site = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/subtitles"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SEARCH))
        .mount(&site)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "link": format!("{}/files/late.srt", site.uri()),
            "remaining": 9
        })))
        .mount(&site)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/late.srt"))
        .respond_with(ResponseTemplate::new(200).set_body_string(srt(&cues(), 1_500)))
        .mount(&site)
        .await;
    site
}

/// Wait for `track`'s sync to be done, and synced.
async fn synced(client: &TestClient, session: &SessionId, track: u32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let timing = tracks(client, session).await.subtitles[track as usize]
            .timing
            .clone()
            .unwrap();
        match timing.sync {
            spoolframe_types::SubtitleSync::Synced { .. } => return,
            spoolframe_types::SubtitleSync::Syncing => {}
            other => panic!("not synced: {other:?}"),
        }
        assert!(std::time::Instant::now() < deadline, "no sync in time");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Each cue of `text` starts where the film's line does.
fn on_the_lines(text: &str) {
    let starts = subsync::cues(text.as_bytes(), TextFormat::Srt).unwrap();
    assert_eq!(starts.len(), cues().len());
    for (got, want) in starts.iter().zip(cues()) {
        assert!(
            (got.start_ms - want.start_ms).abs() <= 200,
            "{got:?} for {want:?}"
        );
    }
}

/// One not found by the file's hash is synced: a player reading it reads
/// the synced timing when the sync is done, without a restart.
#[tokio::test]
async fn a_download_not_matched_by_hash_plays_synced_once_synced() {
    let site = late_site().await;
    let server = ready("subdl-synced", settings(&site, 0)).await;
    let (alice, session) = app(&server, "alice").await;
    search(&alice, &session).await.unwrap();
    let got = download(&alice, &session, "opensubtitles:102")
        .await
        .unwrap();
    let handle = play(&alice, &session, got.track).await;
    synced(&alice, &session, got.track).await;
    match alice
        .call(
            Some(&session),
            Request::ReadSubtitle {
                handle,
                offset: 0,
                version: None,
            },
        )
        .await
    {
        Response::Ok(Reply::SubtitleChunk { bytes, .. }) => {
            on_the_lines(&String::from_utf8(bytes).unwrap())
        }
        other => panic!("{other:?}"),
    }
    server.stop().await;
}

/// A download carried into a converted file (the audio encoded for a
/// device that takes no AC-3): ffmpeg, confined, reads it though it is a
/// file of the server's own, which no media tool may open where it is.
#[tokio::test]
async fn a_download_is_carried_into_a_converted_file() {
    let site = site().await;
    let server = film("subdl-carried", settings(&site, 0)).await;
    server.generate_media(10, "libx264", "ac3");
    server
        .add_user("alice", "correct horse", Some("Alice"))
        .await;
    let (alice, session) = app(&server, "alice").await;
    search(&alice, &session).await.unwrap();
    let got = download(&alice, &session, "opensubtitles:101")
        .await
        .unwrap();
    let handle = match alice
        .call(
            Some(&session),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection {
                    audio: Some(0),
                    subtitle: Some(got.track),
                }),
                start_ms: None,
                bitmap: BitmapSubtitles::Auto,
                quality: Default::default(),
                display: None,
                capabilities: None,
                onward_share: None,
            },
        )
        .await
    {
        Response::Ok(Reply::PlaybackStarted { handle, plan, .. }) => {
            assert!(
                matches!(
                    &plan,
                    spoolframe_protocol::PlaybackPlan::Converted {
                        subtitle: Some(spoolframe_protocol::ConvertedSubtitle {
                            action: spoolframe_protocol::SubtitleAction::Copy { .. },
                            ..
                        }),
                        ..
                    }
                ),
                "not carried: {plan:?}"
            );
            handle
        }
        other => panic!("playback did not start: {other:?}"),
    };
    let mut read = 0u64;
    loop {
        let (response, chunk) = alice
            .read_at(&session, &handle, 0, read, spoolframe_protocol::MAX_READ)
            .await;
        let spoolframe_protocol::MediaReadResponse::Ok(reply) = response else {
            panic!("the conversion failed: {response:?}");
        };
        read += chunk.len() as u64;
        if reply.complete && read == reply.size {
            break;
        }
    }
    assert!(read > 0);
    server.stop().await;
}

/// Carried into a converted file once synced, it is carried synced.
#[tokio::test]
async fn a_synced_download_is_carried_synced() {
    let site = late_site().await;
    let server = film("subdl-carried-synced", settings(&site, 0)).await;
    server.generate_media(10, "libx264", "ac3");
    server
        .add_user("alice", "correct horse", Some("Alice"))
        .await;
    let (alice, session) = app(&server, "alice").await;
    search(&alice, &session).await.unwrap();
    let got = download(&alice, &session, "opensubtitles:102")
        .await
        .unwrap();
    synced(&alice, &session, got.track).await;
    let handle = play(&alice, &session, got.track).await;
    let mut bytes = Vec::new();
    loop {
        let (response, chunk) = alice
            .read_at(
                &session,
                &handle,
                0,
                bytes.len() as u64,
                spoolframe_protocol::MAX_READ,
            )
            .await;
        let spoolframe_protocol::MediaReadResponse::Ok(reply) = response else {
            panic!("the conversion failed: {response:?}");
        };
        bytes.extend(chunk);
        if reply.complete && bytes.len() as u64 == reply.size {
            break;
        }
    }
    let out = server.dir.join("carried.mkv");
    std::fs::write(&out, &bytes).unwrap();
    let text = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&out)
        .args(["-map", "0:s:0", "-f", "srt", "-"])
        .output()
        .unwrap();
    assert!(
        text.status.success(),
        "{}",
        String::from_utf8_lossy(&text.stderr)
    );
    on_the_lines(&String::from_utf8(text.stdout).unwrap());
    server.stop().await;
}

/// Nimble, 0.12.0, run with `--state-dir .dev-state`: a download synced,
/// its offset set and set back, then played over HLS. The conversion
/// copies the server's own file into its job and links to the copy; with
/// a relative state directory the link's target was relative, read from
/// the link's own directory, and ffmpeg found nothing ("Error opening
/// input files"). Every play of the track failed from then on.
#[tokio::test]
async fn a_download_offset_and_reset_plays_over_hls_from_a_relative_state_dir() {
    let site = late_site().await;
    let dir = support::fresh_relative_dir("subdl-relative-hls").await;
    assert!(dir.is_relative());
    let server = film_in(dir, settings(&site, 0)).await;
    server
        .add_user("alice", "correct horse", Some("Alice"))
        .await;
    let capabilities = ClientCapabilities {
        subtitles: [SubtitleFormat::WebVtt].into(),
        ..support::hls_capabilities()
    };
    let (alice, session) = app_with(&server, "alice", capabilities).await;
    search(&alice, &session).await.unwrap();
    let got = download(&alice, &session, "opensubtitles:102")
        .await
        .unwrap();
    synced(&alice, &session, got.track).await;
    for offset_ms in [500, 0] {
        let set = alice
            .call(
                Some(&session),
                Request::SetSubtitleOffset {
                    id: movie().id().clone(),
                    track: got.track,
                    offset_ms,
                },
            )
            .await;
        assert!(
            matches!(&set, Response::Ok(Reply::SubtitleTiming(t)) if t.offset_ms == offset_ms),
            "{set:?}"
        );
    }
    let handle = play(&alice, &session, got.track).await;
    let (response, bytes) = alice
        .media(spoolframe_protocol::MediaRead {
            session: session.clone(),
            handle: handle.clone(),
            offset: 0,
            length: spoolframe_protocol::MAX_READ,
            epoch: 0,
            segment: Some(0),
            variant: 0,
        })
        .await;
    assert!(
        matches!(response, spoolframe_protocol::MediaReadResponse::Ok(_)) && !bytes.is_empty(),
        "{response:?}"
    );
    server.stop().await;
}

/// The SDK's calls, as an app makes them: search, download, the track
/// marked in the item's tracks, remove.
#[tokio::test]
async fn an_app_finds_downloads_and_removes_through_the_sdk() {
    let site = site().await;
    let server = ready("subdl-sdk", settings(&site, 0)).await;
    let storage = std::sync::Arc::new(spoolframe_client_sdk::MemoryStorage::default());
    let client = support::sdk_client(
        storage,
        spoolframe_client_sdk::Device {
            name: "desk".into(),
            client_name: "sdk-test".into(),
            client_version: "0".into(),
            platform: spoolframe_types::Platform::Linux,
            capabilities: support::mkv_capabilities(),
        },
    )
    .await
    .unwrap();
    let server_id = server.server.id().to_string();
    client
        .add_address(
            &server_id,
            server.server.addr().ip_addrs().copied().collect(),
        )
        .unwrap();
    client.connect(&server_id).await.unwrap();
    client.pin(&server_id, true).await.unwrap();
    let alice = client
        .sign_in(&server_id, "alice", "correct horse", false)
        .await
        .unwrap();
    let id = movie().id().clone();
    let found = alice.search_subtitles(&id, Some("en")).await.unwrap();
    assert_eq!(found.languages, ["en"]);
    let got = alice
        .download_subtitle(&id, "opensubtitles:104")
        .await
        .unwrap();
    let tracks = alice.tracks(&id).await.unwrap();
    let download = tracks.subtitles[got.track as usize]
        .download
        .clone()
        .expect("marked as downloaded");
    assert_eq!(download.origin, spoolframe_types::SubtitleOrigin::Ai);
    alice
        .remove_subtitle_download(&id, &download.id)
        .await
        .unwrap();
    assert!(alice.tracks(&id).await.unwrap().subtitles.is_empty());
    server.stop().await;
}

/// `site` answering as SubDL too: its recorded search at
/// `/subdl/api/v1`, and result 3172974-3188291 as a zip of the CP-1251
/// SRT. The settings with both sites.
async fn with_subdl(site: &MockServer) -> Settings {
    use std::io::Write;
    Mock::given(method("GET"))
        .and(path("/subdl/api/v1/subtitles"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(include_str!("../fixtures/subdl/search.json")),
        )
        .mount(site)
        .await;
    let (cp1251, _, _) = encoding_rs::WINDOWS_1251.encode(RUSSIAN);
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file("Film.2019.srt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(&cp1251).unwrap();
    Mock::given(method("GET"))
        .and(path("/subtitle/3172974-3188291.zip"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip.finish().unwrap().into_inner()))
        .mount(site)
        .await;
    let mut sites = settings(site, 0);
    sites.sites.push(SiteSettings {
        languages: vec!["en".into()],
        kind: SiteKind::SubDl(spoolframe_server::subdl::Account {
            api_url: url::Url::parse(&format!("{}/subdl/api/v1", site.uri())).unwrap(),
            download_url: url::Url::parse(&site.uri()).unwrap(),
            api_key: "subdl-key".into(),
        }),
    });
    sites
}

/// A download stays when its site is turned off, live: dropped from
/// `subtitles.sources` or its key taken out. Each site's track is still
/// listed with its source, plays, and its downloader may still remove it.
#[tokio::test]
async fn downloads_stay_when_their_site_is_turned_off() {
    let site = site().await;
    let sites = with_subdl(&site).await;
    let server = ready("subdl-kept", sites.clone()).await;
    let (alice, session) = app(&server, "alice").await;
    let (bob, bob_session) = app(&server, "bob").await;
    search(&alice, &session).await.unwrap();
    let from_os = download(&alice, &session, "opensubtitles:101")
        .await
        .unwrap();
    let from_subdl = download(&alice, &session, "subdl:3172974-3188291")
        .await
        .unwrap();

    // SubDL out of `subtitles.sources`, then no site at all.
    let downloads = &server.debug.subtitle_downloads;
    downloads.configure(Settings {
        sites: vec![sites.sites[0].clone()],
        daily_limit_per_user: 0,
        daily_limit_per_peer: 0,
    });
    downloads.configure(Settings::default());
    assert_eq!(search(&alice, &session).await, Err(Error::NoSubtitleSource));

    let seen = tracks(&bob, &bob_session).await;
    for (got, source) in [(&from_os, "opensubtitles"), (&from_subdl, "subdl")] {
        let track = &seen.subtitles[got.track as usize];
        let download = track.download.clone().expect("still a download");
        assert_eq!(download.source, source);
        assert!(!download.can_remove, "Bob did not download it");
        let handle = play(&bob, &bob_session, got.track).await;
        match bob
            .call(
                Some(&bob_session),
                Request::ReadSubtitle {
                    handle,
                    offset: 0,
                    version: None,
                },
            )
            .await
        {
            Response::Ok(Reply::SubtitleChunk { bytes, .. }) => {
                assert_eq!(String::from_utf8(bytes).unwrap(), RUSSIAN);
            }
            other => panic!("{other:?}"),
        }
    }
    let mine = tracks(&alice, &session).await;
    let subdl_download = mine.subtitles[from_subdl.track as usize]
        .download
        .clone()
        .unwrap();
    assert!(subdl_download.can_remove);
    assert_eq!(
        remove(&alice, &session, &subdl_download.id).await,
        Response::Ok(Reply::Done)
    );
    assert_eq!(tracks(&alice, &session).await.subtitles.len(), 1);
    server.stop().await;
}

/// Both sites on: one search over the wire lists both sites' results and
/// how each went; a SubDL result, zipped, is downloaded from SubDL and is
/// a track named for it, its download saying so.
#[tokio::test]
async fn both_sites_answer_one_search_and_a_subdl_result_is_a_track() {
    let site = site().await;
    let sites = with_subdl(&site).await;
    let server = ready("subdl-both", sites).await;
    let (alice, session) = app(&server, "alice").await;

    let found = search(&alice, &session).await.unwrap();
    let sources: Vec<(&str, &spoolframe_types::SubtitleSourceState)> = found
        .sources
        .iter()
        .map(|s| (s.source.as_str(), &s.state))
        .collect();
    assert_eq!(
        sources,
        [
            (
                "opensubtitles",
                &spoolframe_types::SubtitleSourceState::Found { found: 4 }
            ),
            (
                "subdl",
                &spoolframe_types::SubtitleSourceState::Found { found: 3 }
            ),
        ]
    );
    assert_eq!(found.results.len(), 7);
    assert!(
        found
            .results
            .iter()
            .any(|r| r.id == "subdl:3172974-3188291" && r.source == "subdl")
    );

    let got = download(&alice, &session, "subdl:3172974-3188291")
        .await
        .unwrap();
    assert_eq!(got.account_remaining, None);
    let track = &tracks(&alice, &session).await.subtitles[got.track as usize];
    assert_eq!(track.title.as_deref(), Some("SubDL"));
    assert_eq!(
        track.download.as_ref().map(|d| d.source.as_str()),
        Some("subdl")
    );
    server.stop().await;
}
