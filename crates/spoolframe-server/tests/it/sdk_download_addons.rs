//! The SDK's download add-ons (feature `spoolframe:download-addons`): an
//! iPhone's download of the film gains a subtitle as WebVTT, fetched in
//! the call, and audio tracks as M4As, which the download manager moves
//! as it moves a film: queued, going on from what arrived after a cut,
//! kept in the download's directory and gone with it.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::gate::DownloadAddons;
use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{
    AddonFormat, AddonKind, AddonState, Client, ClientOptions, Device, DownloadEvent,
    DownloadPhase, DownloadSource, DownloadTrackSource, DownloadTracks, LocalDownload,
    MemoryStorage, Session,
};
use spoolframe_test_dir::TestDir;
use spoolframe_types::{AudioCodec, Container, VideoCodec};

use crate::download_addons::write_offset_film;
use crate::support::{ServerOptions, TestServer, fresh_dir, movie, pinned_sdk_client, settled};

const PASSWORD: &str = "correct horse";

fn iphone() -> Device {
    Device {
        name: "Phone".into(),
        client_name: "sdk-test".into(),
        client_version: "0".into(),
        platform: Platform::Ios,
        capabilities: ClientCapabilities {
            video: [VideoCodec::H264].into(),
            audio: [AudioCodec::Aac].into(),
            containers: [Container::Mp4].into(),
            ..ClientCapabilities::default()
        },
    }
}

async fn server(test: &str) -> TestServer {
    let server = TestServer::start_with(
        fresh_dir(test).await,
        ServerOptions {
            real_media: true,
            detect_encoders: true,
            ..ServerOptions::default()
        },
    )
    .await;
    server.add_user("alice", PASSWORD, None).await;
    server
}

/// The iPhone over `storage`, keeping downloads in `dir`, with `server`
/// pinned.
async fn phone(server: &TestServer, storage: &Arc<MemoryStorage>, dir: &Path) -> Client {
    pinned_sdk_client(
        server,
        storage,
        iphone(),
        ClientOptions {
            lan_discovery: false,
            relays: false,
            downloads_dir: Some(dir.to_path_buf()),
            ..ClientOptions::default()
        },
    )
    .await
}

/// The film downloaded with its audio track `audio` alone, done.
async fn downloaded(client: &Client, session: &Session, audio: u32) -> LocalDownload {
    let downloads = client.downloads();
    let started = downloads
        .start(
            session,
            movie().id(),
            DownloadSource::Original,
            DownloadTracks::Chosen {
                audio: vec![audio],
                subtitles: Vec::new(),
            },
        )
        .await
        .unwrap();
    settled(client).await;
    let done = downloads.get(&started.key).unwrap();
    assert_eq!(done.phase, DownloadPhase::Done);
    done
}

fn addon(d: &LocalDownload, kind: AddonKind, place: u32) -> &spoolframe_client_sdk::DownloadAddon {
    d.addons
        .iter()
        .find(|a| a.kind == kind && a.place == place)
        .expect("the add-on is listed")
}

/// The DTS track is queued and the call returns before it lands; the
/// manager converts and fetches it. The subtitle is fetched in the call.
/// Both are told, listed with the file's tracks, and go with the download.
#[tokio::test]
async fn audio_is_queued_for_the_manager_and_a_subtitle_fetched_in_the_call() {
    let server = server("sdk-download-addons").await;
    write_offset_film(&server);
    let dir = TestDir::new("spoolframe-sdk-download-addons").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = phone(&server, &storage, &dir).await;
    let id = server.server.id().to_string();
    let session = client.sign_in(&id, "alice", PASSWORD, false).await.unwrap();
    let key = downloaded(&client, &session, 1).await.key;
    let downloads = client.downloads();
    let mut events = client.download_events();
    let on = session.feature::<DownloadAddons>().unwrap();

    let queued = downloads
        .add_tracks(&key, &on, vec![0], Vec::new())
        .await
        .unwrap();
    let state = addon(&queued, AddonKind::Audio, 0).state.clone();
    assert!(
        matches!(state, AddonState::Preparing { .. }),
        "the audio was not left to the manager: {state:?}"
    );
    let with_subtitle = downloads
        .add_tracks(&key, &on, Vec::new(), vec![0])
        .await
        .unwrap();
    assert_eq!(
        addon(&with_subtitle, AddonKind::Subtitle, 0).state,
        AddonState::Ready
    );
    settled(&client).await;
    let done = downloads.get(&key).unwrap();
    let ready: Vec<_> = done
        .addons
        .iter()
        .map(|a| (a.kind, a.place, a.format, a.state.clone()))
        .collect();
    assert_eq!(
        ready,
        [
            (AddonKind::Audio, 0, AddonFormat::M4a, AddonState::Ready),
            (
                AddonKind::Subtitle,
                0,
                AddonFormat::WebVtt,
                AddonState::Ready
            ),
        ]
    );
    let path =
        |kind| std::path::PathBuf::from(addon(&done, kind, 0).path.clone().expect("a ready file"));
    let (audio, subtitle) = (path(AddonKind::Audio), path(AddonKind::Subtitle));
    let text = std::fs::read_to_string(&subtitle).unwrap();
    let mut told = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let DownloadEvent::Changed(d) = event
            && d.key == key
        {
            told.push(addon(&d, AddonKind::Audio, 0).state.clone());
        }
    }
    let listed: Vec<_> = done
        .tracks_with_addons()
        .into_iter()
        .map(|t| {
            let added = matches!(t.source, DownloadTrackSource::Addon { .. });
            (t.kind, t.place, added)
        })
        .collect();
    assert_eq!(
        (
            audio.starts_with(dir.as_ref()) && audio.exists(),
            text.lines().find(|l| l.contains("-->")),
            told.iter()
                .any(|s| matches!(s, AddonState::Transferring { .. })),
            told.last(),
            listed,
            server.store.downloads().await.unwrap().len(),
        ),
        (
            true,
            Some("00:00:10.500 --> 00:00:11.500"),
            true,
            Some(&AddonState::Ready),
            vec![
                (AddonKind::Audio, 1, false),
                (AddonKind::Audio, 0, true),
                (AddonKind::Subtitle, 0, true),
            ],
            0,
        )
    );

    // Deleted, the download takes its add-ons with it.
    downloads.delete(&key).await.unwrap();
    assert!(!audio.exists() && !subtitle.exists());
    client.close().await;
    server.stop().await;
}

/// The film: 6 minutes of a tiny picture with an AAC track and a FLAC
/// track of noise (some 50 MB, several reads), which an iPhone plays
/// as it is, so its add-on is the track copied into an M4A.
fn write_long_flac_film(server: &TestServer) {
    std::fs::create_dir_all(server.media.parent().unwrap()).unwrap();
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=64x64:rate=1"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "anoisesrc=sample_rate=48000:amplitude=0.5",
        ])
        .args(["-map", "0:v", "-map", "1:a", "-map", "2:a"])
        .args(["-t", "360", "-pix_fmt", "yuv420p", "-c:v", "libx264"])
        .args(["-c:a:0", "aac", "-c:a:1", "flac", "-ac:a:1", "2"])
        .arg(&server.media)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg could not make the film");
}

/// Cut off part way (the app quit), an audio add-on goes on from the
/// bytes kept when the app opens again: the server sends only the rest.
#[tokio::test]
async fn an_audio_add_on_cut_off_goes_on_from_what_arrived() {
    let server = server("sdk-download-addons-resume").await;
    write_long_flac_film(&server);
    let dir = TestDir::new("spoolframe-sdk-download-addons-resume").await;
    let storage = Arc::new(MemoryStorage::default());
    let first = phone(&server, &storage, &dir).await;
    let id = server.server.id().to_string();
    let session = first.sign_in(&id, "alice", PASSWORD, false).await.unwrap();
    let film = downloaded(&first, &session, 0).await;
    let film_bytes = film.size.expect("a done download's size");
    let mut events = first.download_events();
    let on = session.feature::<DownloadAddons>().unwrap();
    first
        .downloads()
        .add_tracks(&film.key, &on, vec![1], Vec::new())
        .await
        .unwrap();
    // The first piece is here: quit.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let size = loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("no piece arrived")
            .unwrap();
        if let DownloadEvent::Changed(d) = event
            && d.key == film.key
            && let a = addon(&d, AddonKind::Audio, 1)
            && a.received > 0
        {
            break a.size.expect("its size, once transferring");
        }
    };
    first.close().await;
    // Cut part way: the track was not all read.
    let read = server.downloads.sent() - film_bytes;
    assert!(read > 0 && read < size, "{read} of {size} bytes read");

    let again = phone(&server, &storage, &dir).await;
    let cut = again.downloads().get(&film.key).unwrap();
    let kept = addon(&cut, AddonKind::Audio, 1).received;
    assert!(kept > 0, "nothing kept of {size} bytes");
    settled(&again).await;
    let done = again.downloads().get(&film.key).unwrap();
    let audio = addon(&done, AddonKind::Audio, 1);
    assert_eq!((&audio.state, audio.received), (&AddonState::Ready, size));
    let file = std::path::PathBuf::from(audio.path.clone().unwrap());
    assert_eq!(std::fs::metadata(&file).unwrap().len(), size);
    // Every byte was read once: the film's, then the track's, with
    // nothing read again after the cut.
    assert_eq!(server.downloads.sent(), film_bytes + size);
    again.close().await;
    server.stop().await;
}

/// Codex, PR #72: an audio add-on the server failed (its conversion could
/// not read the film) was asked about again when added again, and failed
/// again from the server's same answer. Added again, it is prepared anew
/// and comes.
#[tokio::test]
async fn an_audio_add_on_the_server_failed_is_prepared_anew_when_added_again() {
    use std::os::unix::fs::PermissionsExt;
    let server = server("sdk-download-addons-retry").await;
    write_offset_film(&server);
    let dir = TestDir::new("spoolframe-sdk-download-addons-retry").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = phone(&server, &storage, &dir).await;
    let id = server.server.id().to_string();
    let session = client.sign_in(&id, "alice", PASSWORD, false).await.unwrap();
    let key = downloaded(&client, &session, 1).await.key;
    let downloads = client.downloads();
    let on = session.feature::<DownloadAddons>().unwrap();
    let mode = |m| std::fs::set_permissions(&server.media, std::fs::Permissions::from_mode(m));

    mode(0o000).unwrap();
    downloads
        .add_tracks(&key, &on, vec![0], Vec::new())
        .await
        .unwrap();
    settled(&client).await;
    let failed = addon(&downloads.get(&key).unwrap(), AddonKind::Audio, 0)
        .state
        .clone();
    mode(0o644).unwrap();
    downloads
        .add_tracks(&key, &on, vec![0], Vec::new())
        .await
        .unwrap();
    settled(&client).await;
    let then = addon(&downloads.get(&key).unwrap(), AddonKind::Audio, 0)
        .state
        .clone();
    assert_eq!(
        (
            matches!(
                failed,
                AddonState::Failed {
                    cause: spoolframe_client_sdk::DownloadFailure::ServerStoppedConverting,
                    ..
                }
            ),
            &then
        ),
        (true, &AddonState::Ready),
        "{failed:?} then {then:?}"
    );
    client.close().await;
    server.stop().await;
}

/// The server's files of its downloads, once each is ready, removed as
/// they are a day after their last read (the store's clock moved on past
/// it): each such download is failed on the server.
async fn expired(server: &TestServer) {
    use spoolframe_server::store::downloads::StoredState;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let rows = server.store.downloads().await.unwrap();
        if !rows.is_empty()
            && rows
                .iter()
                .all(|r| matches!(r.state, StoredState::Ready { .. }))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the server's file was never made"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server.clock.advance(Duration::from_secs(25 * 60 * 60));
    // A read under way keeps the file until it ends: swept until it goes.
    loop {
        server.downloads.sweep().await.unwrap();
        let rows = server.store.downloads().await.unwrap();
        if rows
            .iter()
            .all(|r| matches!(r.state, StoredState::Failed { .. }))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the server's file was never removed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Start the film's download (its FLAC track) on `client`, and wait
/// until its first piece is here: transferring.
async fn transferring(client: &Client, session: &Session) -> String {
    let mut events = client.download_events();
    let key = client
        .downloads()
        .start(
            session,
            movie().id(),
            DownloadSource::Original,
            DownloadTracks::Chosen {
                audio: vec![1],
                subtitles: Vec::new(),
            },
        )
        .await
        .unwrap()
        .key;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("no piece arrived")
            .unwrap();
        if let DownloadEvent::Changed(d) = event
            && d.key == key
            && d.phase == DownloadPhase::Transferring
            && d.received > 0
        {
            return key;
        }
    }
}

/// sf-ios on Nimble: a download paused while it came, whose server file
/// was removed a day after its last read while it was paused, failed at
/// once on Resume ("prepare it again"). Resumed, it is prepared anew and
/// arrives, as on Retry.
#[tokio::test]
async fn a_paused_download_whose_server_file_expired_is_prepared_anew_on_resume() {
    let server = server("sdk-download-resume-expired").await;
    write_long_flac_film(&server);
    let dir = TestDir::new("spoolframe-sdk-download-resume-expired").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = phone(&server, &storage, &dir).await;
    let id = server.server.id().to_string();
    let session = client.sign_in(&id, "alice", PASSWORD, false).await.unwrap();
    let key = transferring(&client, &session).await;
    let downloads = client.downloads();
    downloads.pause(&key).await.unwrap();
    expired(&server).await;
    downloads.resume(&key).await.unwrap();
    settled(&client).await;
    let then = downloads.get(&key).unwrap().phase;
    assert_eq!(then, DownloadPhase::Done, "{then:?}");
    client.close().await;
    server.stop().await;
}

/// The manifest of download `key` under `dir`, as JSON, and where it is.
fn manifest(dir: &Path, key: &str) -> (std::path::PathBuf, serde_json::Value) {
    fn find(dir: &Path, key: &str) -> Option<(std::path::PathBuf, serde_json::Value)> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = find(&path, key) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|n| n == "manifest.json") {
                let json: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                if json["key"] == key {
                    return Some((path, json));
                }
            }
        }
        None
    }
    find(dir, key).expect("the download's manifest")
}

/// Asked about with no one asking (the app opened again on a download
/// left preparing), a download the server failed stays failed: it is not
/// prepared anew behind the user's back. Retry, which the user asks for,
/// prepares it anew, and it arrives.
#[tokio::test]
async fn a_server_failure_with_no_one_asking_stays_failed_until_retry() {
    let server = server("sdk-download-retry-expired").await;
    write_long_flac_film(&server);
    let dir = TestDir::new("spoolframe-sdk-download-retry-expired").await;
    let storage = Arc::new(MemoryStorage::default());
    let first = phone(&server, &storage, &dir).await;
    let id = server.server.id().to_string();
    let session = first.sign_in(&id, "alice", PASSWORD, false).await.unwrap();
    let key = transferring(&first, &session).await;
    first.downloads().pause(&key).await.unwrap();
    first.close().await;
    expired(&server).await;
    // Left preparing, as by a check the app made by itself, no Resume.
    let (path, mut json) = manifest(&dir, &key);
    json["phase"] = serde_json::to_value(DownloadPhase::Preparing {
        server: spoolframe_client_sdk::DownloadState::Waiting,
    })
    .unwrap();
    std::fs::write(&path, json.to_string()).unwrap();

    let again = phone(&server, &storage, &dir).await;
    settled(&again).await;
    let downloads = again.downloads();
    let failed = downloads.get(&key).unwrap().phase;
    downloads.retry(&key).await.unwrap();
    settled(&again).await;
    let then = downloads.get(&key).unwrap().phase;
    assert_eq!(
        (
            matches!(&failed, DownloadPhase::Failed { reason, .. } if reason.contains("a day after")),
            &then
        ),
        (true, &DownloadPhase::Done),
        "{failed:?} then {then:?}"
    );
    again.close().await;
    server.stop().await;
}

/// A download whose server file is removed while it comes (the app was
/// closed part way, and opened again a day later) is prepared anew as the
/// next read finds the file gone, and arrives: it does not fail.
#[tokio::test]
async fn a_download_whose_server_file_went_mid_transfer_is_prepared_anew() {
    let server = server("sdk-download-vanished").await;
    write_long_flac_film(&server);
    let dir = TestDir::new("spoolframe-sdk-download-vanished").await;
    let storage = Arc::new(MemoryStorage::default());
    let first = phone(&server, &storage, &dir).await;
    let id = server.server.id().to_string();
    let session = first.sign_in(&id, "alice", PASSWORD, false).await.unwrap();
    let mut events = first.download_events();
    let key = first
        .downloads()
        .start(
            &session,
            movie().id(),
            DownloadSource::Original,
            DownloadTracks::Chosen {
                audio: vec![1],
                subtitles: Vec::new(),
            },
        )
        .await
        .unwrap()
        .key;
    // The first piece is here: quit.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("no piece arrived")
            .unwrap();
        if let DownloadEvent::Changed(d) = event
            && d.key == key
            && d.phase == DownloadPhase::Transferring
            && d.received > 0
        {
            break;
        }
    }
    first.close().await;
    expired(&server).await;

    let again = phone(&server, &storage, &dir).await;
    assert_eq!(
        again.downloads().get(&key).unwrap().phase,
        DownloadPhase::Transferring
    );
    settled(&again).await;
    let done = again.downloads().get(&key).unwrap();
    assert_eq!(done.phase, DownloadPhase::Done, "{:?}", done.phase);
    again.close().await;
    server.stop().await;
}
