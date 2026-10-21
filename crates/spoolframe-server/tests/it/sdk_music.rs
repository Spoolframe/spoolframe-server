//! The SDK's music offline (PX-51): downloads of an album, an artist and a
//! playlist, tracks kept from listening under the cache's cap, the
//! network gate, plays made with no server, and playlists and Liked
//! followed while connected, against a server with
//! real ffmpeg and ffprobe and FLAC tracks made at test time. A Linux
//! device plays FLAC as it is, so a track plays direct and its original
//! is the file itself.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Item, MediaId, Platform};
use spoolframe_client_sdk::{
    Client, ClientOptions, ConnectionState, Device, DownloadPhase, DownloadSettings, Features,
    MAX_READ, MemoryStorage, MusicTarget, NetworkKind, OfflineMark, OfflineMusic, PlaybackOptions,
    SdkError, Session,
};
use spoolframe_protocol::playlists::{Kinds, PlaylistEdit, Visibility};
use spoolframe_server::test_support::{album, artist};
use spoolframe_test_dir::TestDir;
use spoolframe_types::{AudioCodec, Container};

use crate::downloads_wire::{PASSWORD, server};
use crate::music_downloads::{alice as alice_actor, track_file, track_id, write_flac};
use crate::support::{ServerOptions, TestServer};

/// A server whose three tracks (32 and 33 of Discovery, 35 of Homework)
/// are FLAC files with a cover, with posters, its music served as a
/// library (Liked), and alice and bob.
async fn music_server(test: &str) -> TestServer {
    let server = server(test, ServerOptions::default()).await;
    server.catalog.show_posters();
    server.catalog.serve_music();
    for key in ["32", "33", "35"] {
        write_flac(&server, key);
    }
    server
}

/// The Linux app: it plays FLAC as it is.
fn linux() -> Device {
    Device {
        name: "Den PC".into(),
        client_name: "sdk-music-test".into(),
        client_version: "0".into(),
        platform: Platform::Linux,
        capabilities: ClientCapabilities {
            audio: [AudioCodec::Flac].into(),
            containers: [Container::Flac].into(),
            ..ClientCapabilities::default()
        },
    }
}

fn options(downloads: &Path, music: bool) -> ClientOptions {
    ClientOptions {
        lan_discovery: false,
        relays: false,
        features: Features {
            downloads: true,
            music_downloads: music,
            playlists: true,
            liked: true,
            ..Features::default()
        },
        downloads_dir: Some(downloads.to_path_buf()),
        ..ClientOptions::default()
    }
}

/// The device over `storage`, keeping downloads in `downloads`, told where
/// `server` is.
async fn device_with(
    server: &TestServer,
    storage: &Arc<MemoryStorage>,
    downloads: &Path,
    music: bool,
) -> Client {
    let client = Client::open_with_options(storage.clone(), linux(), options(downloads, music))
        .await
        .unwrap();
    let id = server.server.id().to_string();
    let addrs = server.server.addr().ip_addrs().copied().collect();
    client.add_address(&id, addrs).unwrap();
    client.connect(&id).await.unwrap();
    client.pin(&id, true).await.unwrap();
    client
}

async fn device(server: &TestServer, storage: &Arc<MemoryStorage>, downloads: &Path) -> Client {
    device_with(server, storage, downloads, true).await
}

async fn alice(server: &TestServer, client: &Client) -> Session {
    client
        .sign_in(&server.server.id().to_string(), "alice", PASSWORD, false)
        .await
        .unwrap()
}

/// Until nothing is left to move, within 2 minutes.
async fn settled(client: &Client) {
    tokio::time::timeout(
        Duration::from_secs(120),
        client.downloads().run_until_done(),
    )
    .await
    .expect("the downloads never settled")
    .unwrap();
}

/// Play track `key` as a player would: `whole`, every byte from the
/// start to the end; else only its first 64 KiB (skipped).
async fn play(session: &Session, key: &str, whole: bool) {
    let playback = session
        .open_playback(&track_id(key), None, PlaybackOptions::default())
        .await
        .unwrap();
    let size = playback.snapshot().size;
    let end = if whole { size } else { (64 << 10).min(size) };
    let mut at = 0;
    while at < end {
        let bytes = playback.read(at, (end - at).min(MAX_READ)).await.unwrap();
        assert!(!bytes.is_empty(), "an empty read at {at} of {size}");
        at += bytes.len() as u64;
    }
    playback.close().await.unwrap();
}

/// The tracks of `music`, by key, with their marks.
fn marks(music: &OfflineMusic) -> Vec<(String, OfflineMark)> {
    let mut marks: Vec<(String, OfflineMark)> = music
        .artists
        .iter()
        .flat_map(|a| &a.albums)
        .flat_map(|a| &a.tracks)
        .map(|t| (t.item.id().key().to_owned(), t.mark))
        .collect();
    marks.sort_by(|a, b| a.0.cmp(&b.0));
    marks
}

fn offline(client: &Client, server: &TestServer, session_user: &str) -> OfflineMusic {
    client
        .downloads()
        .offline_music(&server.server.id().to_string(), session_user)
}

/// The bytes of track `key`'s source file.
fn source(server: &TestServer, key: &str) -> Vec<u8> {
    std::fs::read(track_file(server, key)).unwrap()
}

/// Every byte of each track of an album downloaded is the source's, and
/// with no server the album is there with its tracks, in order, and its
/// cover.
#[tokio::test]
async fn an_album_downloaded_plays_offline_with_its_art() {
    let server = music_server("sdk-music-album").await;
    let dir = TestDir::new("spoolframe-sdk-music-album").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    let user = session.user().user_id.clone();
    let started = client
        .downloads()
        .start_music(
            &session,
            MusicTarget::Album {
                id: album("31").id().clone(),
            },
        )
        .await
        .unwrap();
    settled(&client).await;
    let sources = [source(&server, "32"), source(&server, "33")];
    let server_key = server.server.id().to_string();
    client.close().await;
    server.stop().await;

    let away = Client::open_with_options(storage.clone(), linux(), options(&dir, true))
        .await
        .unwrap();
    let music = away.downloads().offline_music(&server_key, &user);
    let discovery = &music.artists[0].albums[0];
    let files: Vec<Vec<u8>> = discovery
        .tracks
        .iter()
        .map(|t| std::fs::read(&t.file_path).unwrap())
        .collect();
    let cover = discovery.tracks[0]
        .artwork
        .iter()
        .all(|a| std::fs::metadata(&a.path).is_ok_and(|m| m.len() > 0));
    assert_eq!(
        (
            started.name,
            music.artists[0].name.clone(),
            discovery.title.clone(),
            discovery
                .tracks
                .iter()
                .map(|t| t.item.id().key().to_owned())
                .collect::<Vec<_>>(),
            files == sources,
            !discovery.tracks[0].artwork.is_empty() && cover,
            away.downloads().music_downloads()[0].done,
        ),
        (
            "Discovery".to_owned(),
            "Daft Punk".to_owned(),
            "Discovery".to_owned(),
            vec!["32".to_owned(), "33".to_owned()],
            true,
            true,
            2,
        )
    );
    away.close().await;
}

/// A track played to its end is kept, marked as such, the file as it is;
/// one skipped after its first bytes leaves nothing.
#[tokio::test]
async fn a_track_played_to_the_end_is_kept_and_one_skipped_is_not() {
    let server = music_server("sdk-music-kept").await;
    let dir = TestDir::new("spoolframe-sdk-music-kept").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    play(&session, "32", true).await;
    play(&session, "33", false).await;
    let music = offline(&client, &server, &session.user().user_id);
    let kept = &music.artists[0].albums[0].tracks[0];
    let parts = std::fs::read_dir(dir.join("listening"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(
        (
            marks(&music),
            std::fs::read(&kept.file_path).unwrap() == source(&server, "32"),
            parts,
            client.downloads().cache().used,
        ),
        (
            vec![("32".to_owned(), OfflineMark::Kept)],
            true,
            0,
            kept.size,
        )
    );
    client.close().await;
    server.stop().await;
}

/// A device that does not play FLAC gets the track converted, streamed in
/// order: what was streamed, read to its end, is kept, as the encode wrote
/// it.
#[tokio::test]
async fn a_converted_track_streamed_to_its_end_is_kept_as_streamed() {
    let server = music_server("sdk-music-progressive").await;
    let dir = TestDir::new("spoolframe-sdk-music-progressive").await;
    let storage = Arc::new(MemoryStorage::default());
    let aac_only = Device {
        capabilities: ClientCapabilities {
            audio: [AudioCodec::Aac].into(),
            containers: [Container::Matroska].into(),
            delivery: vec![spoolframe_types::Delivery::Progressive],
            ..ClientCapabilities::default()
        },
        ..linux()
    };
    let client = Client::open_with_options(storage.clone(), aac_only, options(&dir, true))
        .await
        .unwrap();
    let id = server.server.id().to_string();
    client
        .add_address(&id, server.server.addr().ip_addrs().copied().collect())
        .unwrap();
    client.connect(&id).await.unwrap();
    client.pin(&id, true).await.unwrap();
    let session = alice(&server, &client).await;
    let playback = session
        .open_playback(&track_id("32"), None, PlaybackOptions::default())
        .await
        .unwrap();
    let source = playback.snapshot();
    let mut streamed = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let chunk = playback
            .read_chunk(&source, streamed.len() as u64, MAX_READ)
            .await
            .unwrap();
        streamed.extend_from_slice(&chunk.bytes);
        if chunk.complete && streamed.len() as u64 == chunk.size {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the encode never ended"
        );
        if chunk.bytes.is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    playback.close().await.unwrap();
    let tracks = client.downloads().music_tracks();
    assert_eq!(
        (
            matches!(
                source.plan,
                spoolframe_client_sdk::PlaybackPlan::Converted { .. }
            ),
            marks(&offline(&client, &server, &session.user().user_id)),
            tracks[0].file.as_ref().map(|f| f.container),
            std::fs::read(tracks[0].file_path.as_ref().unwrap()).unwrap() == streamed,
        ),
        (
            true,
            vec![("32".to_owned(), OfflineMark::Kept)],
            Some(Container::Matroska),
            true,
        )
    );
    client.close().await;
    server.stop().await;
}

/// With the cap full, a new track kept lets the least recently played go;
/// a download, which does not count, stays.
#[tokio::test]
async fn with_the_cache_full_a_new_track_evicts_the_least_recently_played_never_a_download() {
    let server = music_server("sdk-music-cap").await;
    let dir = TestDir::new("spoolframe-sdk-music-cap").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    let downloads = client.downloads();
    downloads
        .start_music(&session, MusicTarget::Track { id: track_id("35") })
        .await
        .unwrap();
    settled(&client).await;
    // Room for one of the two tracks played, not both.
    let (a, b) = (
        source(&server, "32").len() as u64,
        source(&server, "33").len() as u64,
    );
    downloads
        .set_settings(DownloadSettings {
            cache_max_bytes: a.max(b) + 1,
            ..DownloadSettings::default()
        })
        .await
        .unwrap();
    play(&session, "32", true).await;
    let first = marks(&offline(&client, &server, &session.user().user_id));
    // A second later, so the two plays are apart on the clock.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    play(&session, "33", true).await;
    let then = marks(&offline(&client, &server, &session.user().user_id));
    assert_eq!(
        (first, then, downloads.cache().used),
        (
            vec![
                ("32".to_owned(), OfflineMark::Kept),
                ("35".to_owned(), OfflineMark::Download)
            ],
            vec![
                ("33".to_owned(), OfflineMark::Kept),
                ("35".to_owned(), OfflineMark::Download)
            ],
            b,
        )
    );
    client.close().await;
    server.stop().await;
}

/// A kept track downloaded in the quality it was kept in moves to the
/// downloads: the server sends nothing.
#[tokio::test]
async fn downloading_a_kept_track_in_the_same_quality_fetches_nothing() {
    let server = music_server("sdk-music-promote").await;
    let dir = TestDir::new("spoolframe-sdk-music-promote").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    play(&session, "32", true).await;
    let sent = server.downloads.sent();
    client
        .downloads()
        .start_music(&session, MusicTarget::Track { id: track_id("32") })
        .await
        .unwrap();
    settled(&client).await;
    let tracks = client.downloads().music_tracks();
    assert_eq!(
        (
            server.downloads.sent() - sent,
            marks(&offline(&client, &server, &session.user().user_id)),
            tracks.len(),
            tracks[0].phase.clone(),
            std::fs::read(tracks[0].file_path.as_ref().unwrap()).unwrap() == source(&server, "32"),
            client.downloads().cache().used,
        ),
        (
            0,
            vec![("32".to_owned(), OfflineMark::Download)],
            1,
            DownloadPhase::Done,
            true,
            0,
        )
    );
    client.close().await;
    server.stop().await;
}

/// An artist downloaded, then an album of theirs added to the library: it
/// comes down the next time the device reaches the server.
#[tokio::test]
async fn an_album_added_to_a_followed_artist_comes_down_on_the_next_connect() {
    let server = music_server("sdk-music-artist").await;
    let homework = album("34").id().clone();
    server.catalog.not_yet(&homework);
    let dir = TestDir::new("spoolframe-sdk-music-artist").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    let user = session.user().user_id.clone();
    client
        .downloads()
        .start_music(
            &session,
            MusicTarget::Artist {
                id: artist().id().clone(),
            },
        )
        .await
        .unwrap();
    settled(&client).await;
    let before = marks(&offline(&client, &server, &user));
    client.close().await;

    server.catalog.gain(&homework);
    let again = device(&server, &storage, &dir).await;
    // Reaching the server: a session found again.
    alice(&server, &again).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        settled(&again).await;
        if marks(&offline(&again, &server, &user)).len() == 3 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Homework never came down"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let music = offline(&again, &server, &user);
    assert_eq!(
        (
            before.len(),
            marks(&music)
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>(),
            music.artists[0]
                .albums
                .iter()
                .map(|a| a.title.clone())
                .collect::<Vec<_>>(),
        ),
        (
            2,
            vec!["32".to_owned(), "33".to_owned(), "35".to_owned()],
            vec!["Homework".to_owned(), "Discovery".to_owned()],
        )
    );
    again.close().await;
    server.stop().await;
}

/// A track added to a downloaded playlist comes down; one removed goes,
/// unless its album is downloaded too.
#[tokio::test]
async fn a_downloaded_playlist_s_added_track_comes_down_and_a_removed_one_goes_unless_held() {
    let server = music_server("sdk-music-playlist").await;
    let actor = alice_actor(&server).await;
    let playlists = &server.playlists;
    let made = playlists
        .create(
            &actor,
            "Road trip",
            Kinds {
                tracks: true,
                ..Kinds::default()
            },
            Visibility::Shared { with: vec![] },
        )
        .await
        .unwrap();
    let add = |key: &str| PlaylistEdit::Add {
        id: track_id(key),
        before: None,
    };
    let revision = playlists
        .edit(&actor, &made.id, made.revision, vec![add("32")])
        .await
        .unwrap();
    let dir = TestDir::new("spoolframe-sdk-music-playlist").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    let user = session.user().user_id.clone();
    let downloads = client.downloads();
    downloads
        .start_music(
            &session,
            MusicTarget::Album {
                id: album("31").id().clone(),
            },
        )
        .await
        .unwrap();
    downloads
        .start_music(
            &session,
            MusicTarget::Playlist {
                id: made.id.clone(),
            },
        )
        .await
        .unwrap();
    settled(&client).await;
    let kept = || {
        let mut keys: Vec<String> = downloads
            .music_tracks()
            .into_iter()
            .filter(|t| t.phase == DownloadPhase::Done)
            .map(|t| t.item.id().key().to_owned())
            .collect();
        keys.sort();
        keys
    };
    let first = kept();

    // 35 added, 32 (also on the album) removed.
    let entry_of = |key: &'static str| {
        let store = server.store.clone();
        let id = made.id.clone();
        async move {
            store
                .playlist_entries(&id)
                .await
                .unwrap()
                .into_iter()
                .find(|e| e.media_id == track_id(key).to_string())
                .unwrap()
                .entry_id
        }
    };
    let revision = playlists
        .edit(
            &actor,
            &made.id,
            revision,
            vec![
                add("35"),
                PlaylistEdit::Remove {
                    entry: entry_of("32").await,
                },
            ],
        )
        .await
        .unwrap();
    downloads.sync_music(&session).await.unwrap();
    settled(&client).await;
    let changed = kept();
    let listed = offline(&client, &server, &user).playlists[0]
        .tracks
        .iter()
        .map(|t| t.item.id().key().to_owned())
        .collect::<Vec<_>>();

    // 35, held by the playlist alone, removed.
    playlists
        .edit(
            &actor,
            &made.id,
            revision,
            vec![PlaylistEdit::Remove {
                entry: entry_of("35").await,
            }],
        )
        .await
        .unwrap();
    downloads.sync_music(&session).await.unwrap();
    assert_eq!(
        (first, changed, listed, kept()),
        (
            vec!["32".to_owned(), "33".to_owned()],
            vec!["32".to_owned(), "33".to_owned(), "35".to_owned()],
            vec!["35".to_owned()],
            vec!["32".to_owned(), "33".to_owned()],
        )
    );
    client.close().await;
    server.stop().await;
}

/// On a mobile network with cellular downloads off nothing is fetched; on
/// Wi-Fi again the download goes on and finishes.
#[tokio::test]
async fn with_cellular_off_nothing_downloads_and_it_resumes_on_unmetered() {
    let server = music_server("sdk-music-cellular").await;
    let dir = TestDir::new("spoolframe-sdk-music-cellular").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    let downloads = client.downloads();
    downloads.set_network(NetworkKind::Cellular);
    downloads
        .start_music(&session, MusicTarget::Track { id: track_id("32") })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let held = downloads.music_tracks();
    let sent_held = server.downloads.sent();
    downloads.set_network(NetworkKind::Unmetered);
    settled(&client).await;
    let done = downloads.music_tracks();
    assert_eq!(
        (
            sent_held,
            held[0].phase == DownloadPhase::Done,
            held[0].note.is_some(),
            done[0].phase.clone(),
            server.downloads.sent(),
        ),
        (
            0,
            false,
            true,
            DownloadPhase::Done,
            source(&server, "32").len() as u64,
        )
    );
    client.close().await;
    server.stop().await;
}

/// A play and an unlike made with no server reach it the next time the
/// device does: the play in alice's history, once, and the track out of
/// her Liked.
#[tokio::test]
async fn plays_and_likes_made_offline_reach_the_server_on_reconnect() {
    let server = music_server("sdk-music-plays").await;
    let dir = TestDir::new("spoolframe-sdk-music-plays").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    session.like(&[track_id("32")]).await.unwrap();
    client
        .downloads()
        .start_music(&session, MusicTarget::Track { id: track_id("32") })
        .await
        .unwrap();
    settled(&client).await;
    let key = client.downloads().music_tracks()[0].key.clone();
    client.close().await;
    // The unlike below is a minute after the like, by the server's clock.
    server.clock.advance(Duration::from_secs(60));

    // No server (a device that knows of none): the app plays the file and
    // unlikes the track.
    let away = Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        linux(),
        options(&dir, true),
    )
    .await
    .unwrap();
    away.downloads()
        .record_play(&key, Duration::from_secs(200))
        .await
        .unwrap();
    away.downloads().record_like(&key, false).await.unwrap();
    away.close().await;

    let again = device(&server, &storage, &dir).await;
    let session = alice(&server, &again).await;
    // The manager sends them by itself.
    let principal = alice_actor(&server).await.principal;
    let liked = || async {
        server
            .store
            .liked_media(&principal.to_string())
            .await
            .unwrap()
            .contains(&track_id("32").to_string())
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while server.store.all_plays(principal).await.unwrap().is_empty() || liked().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the play and the unlike never arrived"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Asked again (a reply lost, say): still one play.
    again.downloads().sync_music(&session).await.unwrap();
    let plays = server.store.all_plays(principal).await.unwrap();
    assert_eq!(
        (
            plays
                .iter()
                .map(|p| (p.media_id.clone(), p.counted, p.path.clone()))
                .collect::<Vec<_>>(),
            liked().await,
        ),
        (
            vec![(track_id("32").to_string(), true, "offline".to_owned())],
            false,
        )
    );
    again.close().await;
    server.stop().await;
}

/// Without `spoolframe:music-downloads` listed, no music downloads and
/// nothing played is kept.
#[tokio::test]
async fn without_the_feature_there_are_no_music_downloads_and_nothing_is_kept() {
    let server = music_server("sdk-music-unlisted").await;
    let dir = TestDir::new("spoolframe-sdk-music-unlisted").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device_with(&server, &storage, &dir, false).await;
    let session = alice(&server, &client).await;
    let started = client
        .downloads()
        .start_music(&session, MusicTarget::Track { id: track_id("32") })
        .await;
    play(&session, "32", true).await;
    assert_eq!(
        (
            matches!(started, Err(SdkError::Unsupported { .. })),
            client.downloads().music_tracks().len(),
            client.downloads().cache().used,
            dir.join("listening").exists(),
        ),
        (true, 0, 0, false)
    );
    client.close().await;
    server.stop().await;
}

/// Liked downloaded follows it: a track liked later comes down.
#[tokio::test]
async fn liked_downloaded_gains_a_track_liked_later() {
    let server = music_server("sdk-music-liked").await;
    let dir = TestDir::new("spoolframe-sdk-music-liked").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    session.like(&[track_id("33")]).await.unwrap();
    let downloads = client.downloads();
    downloads
        .start_music(&session, MusicTarget::Liked)
        .await
        .unwrap();
    settled(&client).await;
    let first: Vec<MediaId> = downloads
        .music_tracks()
        .iter()
        .map(|t| t.item.id().clone())
        .collect();
    session.like(&[track_id("35")]).await.unwrap();
    downloads.sync_music(&session).await.unwrap();
    settled(&client).await;
    let mut then: Vec<String> = downloads
        .music_tracks()
        .iter()
        .filter(|t| matches!(t.item, Item::Track(_)) && t.phase == DownloadPhase::Done)
        .map(|t| t.item.id().key().to_owned())
        .collect();
    then.sort();
    assert_eq!(
        (first, then),
        (vec![track_id("33")], vec!["33".to_owned(), "35".to_owned()])
    );
    client.close().await;
    server.stop().await;
}

/// Emptying the cache lets go of tracks kept from listening alone: 35,
/// played; 32, played and then downloaded with its album, and 33, on the
/// album and never played, stay as downloads. The downloads are as they
/// were.
#[tokio::test]
async fn clearing_the_cache_lets_go_of_kept_tracks_and_never_a_download() {
    let server = music_server("sdk-music-clear-cache").await;
    let dir = TestDir::new("spoolframe-sdk-music-clear-cache").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    let user = session.user().user_id.clone();
    let downloads = client.downloads();
    play(&session, "35", true).await;
    play(&session, "32", true).await;
    downloads
        .start_music(
            &session,
            MusicTarget::Album {
                id: album("31").id().clone(),
            },
        )
        .await
        .unwrap();
    settled(&client).await;
    let before = offline(&client, &server, &user);
    let file_of = |music: &OfflineMusic, key: &str| {
        music
            .artists
            .iter()
            .flat_map(|a| &a.albums)
            .flat_map(|a| &a.tracks)
            .find(|t| t.item.id().key() == key)
            .unwrap()
            .file_path
            .clone()
    };
    let files = ["35", "32", "33"].map(|key| file_of(&before, key));
    let (used, listed) = (downloads.cache().used, downloads.music_downloads());

    downloads.clear_cache().await.unwrap();
    assert_eq!(
        (
            marks(&before),
            used > 0,
            files.each_ref().map(|f| f.exists()),
            marks(&offline(&client, &server, &user)),
            downloads.cache().used,
            downloads.music_downloads() == listed,
        ),
        (
            vec![
                ("32".to_owned(), OfflineMark::Download),
                ("33".to_owned(), OfflineMark::Download),
                ("35".to_owned(), OfflineMark::Kept),
            ],
            true,
            [false, true, true],
            vec![
                ("32".to_owned(), OfflineMark::Download),
                ("33".to_owned(), OfflineMark::Download),
            ],
            0,
            true,
        )
    );
    client.close().await;
    server.stop().await;
}

/// The tracks whole on the device, by key.
fn whole(client: &Client) -> Vec<String> {
    let mut keys: Vec<String> = client
        .downloads()
        .music_tracks()
        .into_iter()
        .filter(|t| t.phase == DownloadPhase::Done)
        .map(|t| t.item.id().key().to_owned())
        .collect();
    keys.sort();
    keys
}

/// Until the tracks whole on the device are `want`, within 30 s, with no
/// sync asked for: what the device follows by itself while connected.
async fn followed_to(client: &Client, want: &[&str]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let now = whole(client);
        if now == want {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the device has {now:?}, never {want:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Whether `client` is connected to `server` now.
fn connected(client: &Client, server: &TestServer) -> bool {
    client
        .state(&server.server.id().to_string())
        .unwrap()
        .current()
        == ConnectionState::Connected
}

/// Alice's playlist "Road trip", of tracks `keys`: its id and revision.
async fn road_trip(server: &TestServer, keys: &[&str]) -> (String, u64) {
    let actor = alice_actor(server).await;
    let made = server
        .playlists
        .create(
            &actor,
            "Road trip",
            Kinds {
                tracks: true,
                ..Kinds::default()
            },
            Visibility::Shared { with: vec![] },
        )
        .await
        .unwrap();
    let adds = keys
        .iter()
        .map(|key| PlaylistEdit::Add {
            id: track_id(key),
            before: None,
        })
        .collect();
    let revision = server
        .playlists
        .edit(&actor, &made.id, made.revision, adds)
        .await
        .unwrap();
    (made.id, revision)
}

/// A track added to a downloaded playlist on the server comes down while
/// the device stays connected: no reconnect, no sync asked for.
#[tokio::test]
async fn a_track_added_to_a_downloaded_playlist_comes_down_while_connected() {
    let server = music_server("sdk-music-live-add").await;
    let (playlist, revision) = road_trip(&server, &["32"]).await;
    let dir = TestDir::new("spoolframe-sdk-music-live-add").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    let user = session.user().user_id.clone();
    client
        .downloads()
        .start_music(
            &session,
            MusicTarget::Playlist {
                id: playlist.clone(),
            },
        )
        .await
        .unwrap();
    settled(&client).await;
    let first = whole(&client);

    // Added from elsewhere: the server's own edit, not this device's.
    let actor = alice_actor(&server).await;
    server
        .playlists
        .edit(
            &actor,
            &playlist,
            revision,
            vec![PlaylistEdit::Add {
                id: track_id("35"),
                before: None,
            }],
        )
        .await
        .unwrap();
    followed_to(&client, &["32", "35"]).await;
    let listed = &offline(&client, &server, &user).playlists[0];
    assert_eq!(
        (
            first,
            listed
                .tracks
                .iter()
                .map(|t| t.item.id().key().to_owned())
                .collect::<Vec<_>>(),
            std::fs::read(&listed.tracks[1].file_path).unwrap() == source(&server, "35"),
            connected(&client, &server),
        ),
        (
            vec!["32".to_owned()],
            vec!["32".to_owned(), "35".to_owned()],
            true,
            true,
        )
    );
    client.close().await;
    server.stop().await;
}

/// Tracks removed from a downloaded playlist on the server go while the
/// device stays connected: 35, which only the playlist held, is deleted;
/// 32 stays, as its album is downloaded too.
#[tokio::test]
async fn a_track_removed_from_a_downloaded_playlist_goes_while_connected_unless_held() {
    let server = music_server("sdk-music-live-remove").await;
    let (playlist, revision) = road_trip(&server, &["32", "35"]).await;
    let dir = TestDir::new("spoolframe-sdk-music-live-remove").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    let user = session.user().user_id.clone();
    let downloads = client.downloads();
    downloads
        .start_music(
            &session,
            MusicTarget::Album {
                id: album("31").id().clone(),
            },
        )
        .await
        .unwrap();
    downloads
        .start_music(
            &session,
            MusicTarget::Playlist {
                id: playlist.clone(),
            },
        )
        .await
        .unwrap();
    settled(&client).await;
    let first = whole(&client);
    let file_35 = downloads
        .music_tracks()
        .into_iter()
        .find(|t| t.item.id().key() == "35")
        .and_then(|t| t.file_path)
        .unwrap();

    let removes = server
        .store
        .playlist_entries(&playlist)
        .await
        .unwrap()
        .into_iter()
        .map(|e| PlaylistEdit::Remove { entry: e.entry_id })
        .collect();
    let actor = alice_actor(&server).await;
    server
        .playlists
        .edit(&actor, &playlist, revision, removes)
        .await
        .unwrap();
    followed_to(&client, &["32", "33"]).await;
    let music = offline(&client, &server, &user);
    assert_eq!(
        (
            first,
            file_35.exists(),
            music.playlists[0].tracks.len(),
            marks(&music),
            connected(&client, &server),
        ),
        (
            vec!["32".to_owned(), "33".to_owned(), "35".to_owned()],
            false,
            0,
            vec![
                ("32".to_owned(), OfflineMark::Download),
                ("33".to_owned(), OfflineMark::Download),
            ],
            true,
        )
    );
    client.close().await;
    server.stop().await;
}

/// The cursor kept in the device's `music.json` under `dir`.
fn kept_cursor(dir: &Path) -> Option<String> {
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(at) = dirs.pop() {
        for entry in std::fs::read_dir(&at).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.file_name().is_some_and(|n| n == "music.json") {
                let state: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                return state["cursor"].as_str().map(str::to_owned);
            }
        }
    }
    None
}

/// Review 2026-10-20, finding 9: asked about Liked alone (its
/// `LikedChanged`), the device sends its cursor and keeps the one it is
/// given, rather than drop it.
#[tokio::test]
async fn a_live_ask_about_liked_alone_keeps_the_cursor() {
    let server = music_server("sdk-music-live-cursor").await;
    let actor = alice_actor(&server).await;
    server
        .playlists
        .like(&actor, &[track_id("33")])
        .await
        .unwrap();
    let dir = TestDir::new("spoolframe-sdk-music-live-cursor").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    client
        .downloads()
        .start_music(&session, MusicTarget::Liked)
        .await
        .unwrap();
    settled(&client).await;
    server
        .playlists
        .like(&actor, &[track_id("35")])
        .await
        .unwrap();
    followed_to(&client, &["33", "35"]).await;
    assert!(kept_cursor(&dir).is_some(), "the cursor was dropped");
    client.close().await;
    server.stop().await;
}

/// A track liked elsewhere while Liked is downloaded comes down while the
/// device stays connected (`LikedChanged`).
#[tokio::test]
async fn a_track_liked_elsewhere_comes_down_while_connected() {
    let server = music_server("sdk-music-live-liked").await;
    let actor = alice_actor(&server).await;
    server
        .playlists
        .like(&actor, &[track_id("33")])
        .await
        .unwrap();
    let dir = TestDir::new("spoolframe-sdk-music-live-liked").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device(&server, &storage, &dir).await;
    let session = alice(&server, &client).await;
    let user = session.user().user_id.clone();
    client
        .downloads()
        .start_music(&session, MusicTarget::Liked)
        .await
        .unwrap();
    settled(&client).await;
    let first = whole(&client);

    server
        .playlists
        .like(&actor, &[track_id("35")])
        .await
        .unwrap();
    followed_to(&client, &["33", "35"]).await;
    assert_eq!(
        (
            first,
            offline(&client, &server, &user).playlists[0]
                .tracks
                .iter()
                .map(|t| t.item.id().key().to_owned())
                .collect::<Vec<_>>(),
            connected(&client, &server),
        ),
        (
            vec!["33".to_owned()],
            // Liked newest first.
            vec!["35".to_owned(), "33".to_owned()],
            true,
        )
    );
    client.close().await;
    server.stop().await;
}
