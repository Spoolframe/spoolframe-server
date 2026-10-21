//! The SDK's downloads (PX-38): its manager and offline store against a
//! server with real ffmpeg and ffprobe. A Linux device downloads the
//! film as it is, checks it whole, goes on after being cut off, keeps
//! what it needs to show and play it with no server, and sends what was
//! watched offline.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Item, Platform};
use spoolframe_client_sdk::{
    Client, ClientOptions, Device, DownloadEvent, DownloadPhase, DownloadSource, DownloadTracks,
    Features, LocalDownload, MemoryStorage, OfflineWatchOutcome, Session,
};
use spoolframe_protocol::{Hello, PROTOCOL_VERSION, Reply, Request, Response};
use spoolframe_test_dir::TestDir;
use spoolframe_types::{AudioCodec, Container, VideoCodec};

use crate::support::{ServerOptions, TestServer, fresh_dir, movie};

const PASSWORD: &str = "correct horse";

/// A server with real ffmpeg and ffprobe, posters, and alice; its film
/// written as 8 s of H.264 noise and AAC in Matroska: about 60 MB, eight
/// reads.
async fn server(test: &str, options: ServerOptions) -> TestServer {
    let server = TestServer::start_with(
        fresh_dir(test).await,
        ServerOptions {
            real_media: true,
            ..options
        },
    )
    .await;
    server.add_user("alice", PASSWORD, None).await;
    server.catalog.show_posters();
    write_noise_film(&server.media);
    server
}

fn write_noise_film(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "nullsrc=size=640x360:rate=25,geq=random(1)*255:128:128",
        ])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args([
            "-map", "0:v", "-map", "1:a", "-t", "8", "-pix_fmt", "yuv420p",
        ])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-qp", "8"])
        .args(["-c:a", "aac"])
        .arg(path)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg could not make the film");
}

/// The Linux app: it plays the film's H.264 and AAC in Matroska.
fn linux() -> Device {
    Device {
        name: "Den PC".into(),
        client_name: "sdk-test".into(),
        client_version: "0".into(),
        platform: Platform::Linux,
        capabilities: ClientCapabilities {
            video: [VideoCodec::H264].into(),
            audio: [AudioCodec::Aac].into(),
            containers: [Container::Matroska].into(),
            ..ClientCapabilities::default()
        },
    }
}

fn options(downloads: &Path, relays: bool) -> ClientOptions {
    ClientOptions {
        lan_discovery: false,
        relays,
        features: Features {
            downloads: true,
            ..Features::default()
        },
        downloads_dir: Some(downloads.to_path_buf()),
        ..ClientOptions::default()
    }
}

/// The device over `storage` keeping downloads in `downloads`, told
/// where `server` is; it has pinned it once.
async fn device(server: &TestServer, storage: &Arc<MemoryStorage>, downloads: &Path) -> Client {
    let client = Client::open_with_options(storage.clone(), linux(), options(downloads, false))
        .await
        .unwrap();
    let id = server.server.id().to_string();
    let addrs = server.server.addr().ip_addrs().copied().collect();
    client.add_address(&id, addrs).unwrap();
    client.connect(&id).await.unwrap();
    client.pin(&id, true).await.unwrap();
    client
}

async fn alice(server: &TestServer, client: &Client) -> Session {
    client
        .sign_in(&server.server.id().to_string(), "alice", PASSWORD, false)
        .await
        .unwrap()
}

async fn start(client: &Client, session: &Session) -> LocalDownload {
    client
        .downloads()
        .start(
            session,
            movie().id(),
            DownloadSource::Original,
            DownloadTracks::All,
        )
        .await
        .unwrap()
}

/// Download `key` once nothing is left to move, within 2 minutes.
async fn settled(client: &Client, key: &str) -> LocalDownload {
    tokio::time::timeout(
        Duration::from_secs(120),
        client.downloads().run_until_done(),
    )
    .await
    .expect("the download never settled")
    .unwrap();
    client.downloads().get(key).unwrap()
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The film arrives whole as the film's own bytes, its SHA-256 checked,
/// and the server is told it finished: it no longer has the download.
#[tokio::test]
async fn a_download_arrives_whole_checked_and_the_server_lets_it_go() {
    let server = server("sdk-downloads-whole", ServerOptions::default()).await;
    let dir = TestDir::new("spoolframe-sdk-downloads-whole").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    let started = start(&client, &session).await;
    let done = settled(&client, &started.key).await;
    assert_eq!(done.phase, DownloadPhase::Done);
    let path = done
        .file_path
        .clone()
        .expect("a done download has its file");
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("mkv"));
    let film = std::fs::read(&server.media).unwrap();
    let kept = std::fs::read(&path).unwrap();
    assert_eq!(
        (kept.len(), sha256(&kept), done.received, done.size),
        (
            film.len(),
            sha256(&film),
            film.len() as u64,
            Some(film.len() as u64)
        )
    );
    assert_eq!(
        done.file_url.as_deref(),
        Some(url::Url::from_file_path(&path).unwrap().as_str())
    );
    let rows = server.store.downloads().await.unwrap();
    assert!(
        rows.is_empty(),
        "the server still has the download: {rows:?}"
    );
    client.close().await;
    server.stop().await;
}

/// Cut off part way (the app quit), the transfer goes on from what it
/// had when the device opens again: the server sends only the rest.
#[tokio::test]
async fn a_transfer_cut_off_part_way_goes_on_from_where_it_stopped() {
    let server = server("sdk-downloads-resume", ServerOptions::default()).await;
    let dir = TestDir::new("spoolframe-sdk-downloads-resume").await;
    let storage = Arc::new(MemoryStorage::default());
    let size = std::fs::metadata(&server.media).unwrap().len();
    let first = device(&server, &storage, &dir).await;
    let session = alice(&server, &first).await;
    let mut events = first.download_events();
    let started = start(&first, &session).await;
    // The first piece is here: quit. The manager lets go after the read
    // under way, long before the rest could come.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("no piece arrived")
            .unwrap();
        if let DownloadEvent::Changed(d) = event
            && d.key == started.key
            && d.phase == DownloadPhase::Transferring
            && d.received > 0
        {
            break;
        }
    }
    first.close().await;
    let sent_before = server.downloads.sent();

    let again = device(&server, &storage, &dir).await;
    let cut = again.downloads().get(&started.key).unwrap();
    assert_eq!(cut.phase, DownloadPhase::Transferring);
    assert!(
        cut.received > 0 && cut.received < size,
        "{} of {size} bytes kept",
        cut.received
    );
    let done = settled(&again, &started.key).await;
    assert_eq!(done.phase, DownloadPhase::Done);
    let film = std::fs::read(&server.media).unwrap();
    assert_eq!(
        sha256(&std::fs::read(done.file_path.unwrap()).unwrap()),
        sha256(&film)
    );
    // Only what was missing was read again.
    assert_eq!(server.downloads.sent() - sent_before, size - cut.received);
    again.close().await;
    server.stop().await;
}

/// With the server gone, a new start of the app lists the download with
/// all it needs: the film's details and tracks, its poster, and its file.
/// (The film has no markers here; they are kept as the item's details
/// gave them.)
#[tokio::test]
async fn with_no_server_the_list_has_the_details_artwork_and_file() {
    let server = server("sdk-downloads-offline", ServerOptions::default()).await;
    let dir = TestDir::new("spoolframe-sdk-downloads-offline").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    let key = start(&client, &session).await.key;
    settled(&client, &key).await;
    client.close().await;
    let film = std::fs::read(&server.media).unwrap();
    server.stop().await;

    let offline = Client::open_with_options(storage.clone(), linux(), options(&dir, false))
        .await
        .unwrap();
    let listed = offline.downloads().list();
    assert_eq!(listed.len(), 1, "{listed:?}");
    let d = &listed[0];
    let Item::Movie(film_item) = &d.item else {
        panic!("not the film: {:?}", d.item);
    };
    assert_eq!(
        (film_item.title.as_str(), d.username.as_str()),
        ("Alien", "alice")
    );
    assert_eq!(d.tracks.audio.len(), 1, "{:?}", d.tracks);
    assert_eq!(d.artwork.len(), 1, "{:?}", d.artwork);
    let poster = std::fs::read(&d.artwork[0].path).unwrap();
    assert!(!poster.is_empty());
    assert_eq!(
        std::fs::read(d.file_path.as_ref().unwrap()).unwrap().len(),
        film.len()
    );
    offline.close().await;
}

/// A relay on loopback, and its URL.
async fn relay() -> (iroh_relay::server::Server, url::Url) {
    use iroh_relay::server::{RelayConfig, ServerConfig};
    let mut config = ServerConfig::default();
    config.relay = Some(RelayConfig::new((std::net::Ipv4Addr::LOCALHOST, 0)));
    let relay = iroh_relay::server::Server::spawn(config).await.unwrap();
    let addr = relay.http_addr().expect("served over HTTP");
    (relay, url::Url::parse(&format!("http://{addr}")).unwrap())
}

/// A device that knows the server only by a link naming its relay, no
/// address, downloads the film whole: dialled through the relay first.
#[tokio::test]
async fn a_server_known_only_by_its_relay_sends_the_download_whole() {
    let (relay, url) = relay().await;
    let server = server(
        "sdk-downloads-relay",
        ServerOptions {
            relays: vec![spoolframe_server::network::Relay::Custom(url.clone())],
            ..ServerOptions::default()
        },
    )
    .await;
    let home = tokio::time::timeout(Duration::from_secs(10), async {
        let url = iroh::RelayUrl::from(url.clone());
        while !server.server.addr().relay_urls().any(|u| *u == url) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(home.is_ok(), "the server never homed on the relay");
    let dir = TestDir::new("spoolframe-sdk-downloads-relay").await;
    let client = Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        linux(),
        options(&dir, true),
    )
    .await
    .unwrap();
    let link = spoolframe_protocol::invite::Invite {
        server: server.server.id().to_string(),
        relays: server.server.told().relays(),
        ..Default::default()
    }
    .link();
    client.connect_invite(&link).await.unwrap();
    client.save_invite_server(&link).await.unwrap();
    let session = alice(&server, &client).await;
    let key = start(&client, &session).await.key;
    let done = settled(&client, &key).await;
    assert_eq!(done.phase, DownloadPhase::Done);
    assert_eq!(
        sha256(&std::fs::read(done.file_path.unwrap()).unwrap()),
        sha256(&std::fs::read(&server.media).unwrap())
    );
    client.close().await;
    server.stop().await;
    drop(relay);
}

/// Where the film's playback stands on the server, for alice.
async fn on_server(session: &Session) -> spoolframe_client_sdk::types::PlaybackState {
    match session.item(movie().id()).await.unwrap().item {
        Item::Movie(m) => m.playback,
        other => panic!("not the film: {other:?}"),
    }
}

/// Watched to the end offline (past the film's 8 s), the film is watched
/// on the server once the app is open again with a session there: sent
/// by itself.
#[tokio::test]
async fn what_was_watched_offline_reaches_the_server_on_the_next_start() {
    let server = server("sdk-downloads-watch", ServerOptions::default()).await;
    let dir = TestDir::new("spoolframe-sdk-downloads-watch").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    let key = start(&client, &session).await.key;
    settled(&client, &key).await;
    assert!(!on_server(&session).await.watched);
    client.close().await;

    // Watched with the app offline.
    let offline = Client::open_with_options(storage.clone(), linux(), options(&dir, false))
        .await
        .unwrap();
    let watched = offline
        .downloads()
        .record_position(&key, Duration::from_secs(600))
        .await
        .unwrap();
    assert!(watched.watch.pending);
    offline.close().await;

    let back = device(&server, &storage, &dir).await;
    let session = back
        .profiles(&server.server.id().to_string())
        .await
        .unwrap()
        .remove(0);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !on_server(&session).await.watched {
        assert!(
            tokio::time::Instant::now() < deadline,
            "never sent: {:?}",
            on_server(&session).await
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while back.downloads().get(&key).unwrap().watch.pending {
        assert!(tokio::time::Instant::now() < deadline, "still pending");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    back.close().await;
    server.stop().await;
}

/// Watched offline before the user marked the film watched on another
/// device: the server keeps the newer, and the download shows it.
#[tokio::test]
async fn an_older_offline_watch_loses_to_the_newer_one_on_the_server() {
    let server = server("sdk-downloads-older", ServerOptions::default()).await;
    let dir = TestDir::new("spoolframe-sdk-downloads-older").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    let key = start(&client, &session).await.key;
    settled(&client, &key).await;
    client
        .downloads()
        .record_position(&key, Duration::from_secs(600))
        .await
        .unwrap();
    // Seconds are what both sides count in: the mark is a whole second
    // and more later, whatever second boundary the two cross.
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let other = server.client().await;
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "test".into(),
        client_version: "0".into(),
        platform: Platform::Tvos,
        device_name: None,
        capabilities: None,
        experimental: Vec::new(),
        features: Vec::new(),
    };
    other.call(None, Request::Hello(hello)).await;
    let token = other.login("alice", PASSWORD).await;
    assert!(matches!(
        other
            .call(
                Some(&token),
                Request::MarkWatched {
                    id: movie().id().clone()
                }
            )
            .await,
        Response::Ok(Reply::Done)
    ));

    let synced = client.downloads().sync_watch(&session).await.unwrap();
    assert_eq!(
        synced
            .iter()
            .map(|s| (s.item.clone(), s.outcome))
            .collect::<Vec<_>>(),
        [(movie().id().clone(), OfflineWatchOutcome::Older)]
    );
    let state = on_server(&session).await;
    assert!(state.watched, "{state:?}");
    let watch = client.downloads().get(&key).unwrap().watch;
    assert_eq!((watch.watched, watch.pending), (Some(true), false));
    client.close().await;
    server.stop().await;
}

/// Paused, a download stays where it got to, and nothing is left to
/// move; resumed, it arrives whole.
#[tokio::test]
async fn a_paused_download_waits_and_once_resumed_arrives_whole() {
    let server = server("sdk-downloads-pause", ServerOptions::default()).await;
    let dir = TestDir::new("spoolframe-sdk-downloads-pause").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    let key = start(&client, &session).await.key;
    let paused = client.downloads().pause(&key).await.unwrap();
    assert_eq!(paused.phase, DownloadPhase::Paused);
    let still = settled(&client, &key).await;
    assert_eq!(still.phase, DownloadPhase::Paused);
    assert_eq!(still.file_path, None);
    client.downloads().resume(&key).await.unwrap();
    let done = settled(&client, &key).await;
    assert_eq!(done.phase, DownloadPhase::Done);
    assert_eq!(
        sha256(&std::fs::read(done.file_path.unwrap()).unwrap()),
        sha256(&std::fs::read(&server.media).unwrap())
    );
    client.close().await;
    server.stop().await;
}

/// Deleted, a download is gone from the device, files and all, and the
/// app is told.
#[tokio::test]
async fn a_deleted_download_leaves_nothing_on_the_device() {
    let server = server("sdk-downloads-delete", ServerOptions::default()).await;
    let dir = TestDir::new("spoolframe-sdk-downloads-delete").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    let key = start(&client, &session).await.key;
    let file = settled(&client, &key).await.file_path.unwrap();
    let mut events = client.download_events();
    client.downloads().delete(&key).await.unwrap();
    assert_eq!(
        events.recv().await.unwrap(),
        DownloadEvent::Removed { key: key.clone() }
    );
    assert!(client.downloads().list().is_empty());
    assert!(!file.exists());
    assert!(client.downloads().space().unwrap().used < 1 << 20);
    client.close().await;
    server.stop().await;
}

/// Two profiles download the same film: one file, each one's download.
/// Each sees only theirs; the first one deleting it leaves it for the
/// other.
#[tokio::test]
async fn two_profiles_share_one_file_and_each_sees_their_own() {
    let server = server("sdk-downloads-shared", ServerOptions::default()).await;
    server.add_user("bob", PASSWORD, None).await;
    let dir = TestDir::new("spoolframe-sdk-downloads-shared").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let alice = alice(&server, &client).await;
    let bob = client
        .sign_in(&server.server.id().to_string(), "bob", PASSWORD, false)
        .await
        .unwrap();
    let key = start(&client, &alice).await.key;
    let file = settled(&client, &key).await.file_path.unwrap();
    assert_eq!(start(&client, &bob).await.key, key);
    let downloads = client.downloads();
    let id = server.server.id().to_string();
    downloads.set_profile(&id, &bob.user().user_id);
    assert_eq!(downloads.list().len(), 1);
    downloads.set_profile(&id, "local:someone-else");
    assert!(downloads.list().is_empty());
    assert!(downloads.get(&key).is_err());

    downloads.set_profile(&id, &alice.user().user_id);
    downloads.delete(&key).await.unwrap();
    assert!(downloads.list().is_empty());
    assert!(file.exists(), "bob's download went with alice's");
    downloads.set_profile(&id, &bob.user().user_id);
    let left = downloads.list();
    assert_eq!(
        (left.len(), left[0].phase.clone()),
        (1, DownloadPhase::Done)
    );
    downloads.delete(&key).await.unwrap();
    assert!(!file.exists());
    client.close().await;
    server.stop().await;
}

/// The PIN asked for on this device locks the profile's downloads with no
/// server, as it locks the profile online, until its PIN is given (PX-60).
#[tokio::test]
async fn offline_a_profile_whose_pin_is_asked_here_opens_only_with_it() {
    let server = server("sdk-downloads-locked", ServerOptions::default()).await;
    let dir = TestDir::new("spoolframe-sdk-downloads-locked").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let first = alice(&server, &client).await;
    first.set_pin("1234", None).await.unwrap();
    first.set_pin_required(None, true).await.unwrap();
    // Signed in again, the profile starts locked; its PIN opens it.
    let session = alice(&server, &client).await;
    session.unlock("1234").await.unwrap();
    let key = start(&client, &session).await.key;
    settled(&client, &key).await;
    let user_id = session.user().user_id.clone();
    client.close().await;
    let id = server.server.id().to_string();
    server.stop().await;

    let offline = Client::open_with_options(storage.clone(), linux(), options(&dir, false))
        .await
        .unwrap();
    let downloads = offline.downloads();
    downloads.set_profile(&id, &user_id);
    assert!(downloads.profile_locked());
    assert!(downloads.list().is_empty());
    assert!(matches!(
        downloads.get(&key),
        Err(spoolframe_client_sdk::SdkError::ProfileLocked)
    ));
    assert!(matches!(
        downloads.unlock_offline("0000").await,
        Err(spoolframe_client_sdk::SdkError::InvalidCredentials)
    ));
    downloads.unlock_offline("1234").await.unwrap();
    assert_eq!(downloads.list().len(), 1);
    assert!(downloads.get(&key).unwrap().file_path.is_some());
    offline.close().await;
}
