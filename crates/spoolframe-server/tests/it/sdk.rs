//! The client SDK (P1-15) against a real server in-process. The server is
//! off the LAN here; the SDK is given its address, as a typed-in address
//! would be. Discovery is tested in `lan.rs`.

use crate::support;

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Item, PlaybackState, VideoCodec};
use spoolframe_client_sdk::{
    ArtworkSize, BitmapSubtitles, Client, ConnectionState, Device, MAX_READ, ManifestKind,
    MemoryStorage, PlaybackHandle, PlaybackOptions, PlaybackPlan, SdkError, Selection, Session,
    Storage,
};
use support::{
    ServerOptions, TestServer, episode, fresh_dir, library, markers, mkv_capabilities, movie,
    poster, show,
};

fn device(capabilities: ClientCapabilities) -> Device {
    Device {
        name: "living room".into(),
        client_name: "sdk-test".into(),
        client_version: "0".into(),
        capabilities,
    }
}

/// A client on `storage` that knows where `server` is.
async fn client_on(server: &TestServer, storage: &Arc<MemoryStorage>) -> Client {
    client_with(server, storage, mkv_capabilities()).await
}

/// [`client_on`], for a device that can play `capabilities`.
async fn client_with(
    server: &TestServer,
    storage: &Arc<MemoryStorage>,
    capabilities: ClientCapabilities,
) -> Client {
    let client = Client::open(storage.clone(), device(capabilities))
        .await
        .unwrap();
    let addrs = server.server.addr().ip_addrs().copied().collect();
    client
        .add_address(&server.server.id().to_string(), addrs)
        .unwrap();
    pin(&client, server).await;
    client
}

/// Connect and pin, as an app does once the user has checked the
/// fingerprint (P1-21).
async fn pin(client: &Client, server: &TestServer) {
    let entry = client.connect(&id(server)).await.unwrap();
    assert!(!entry.pinned || entry.verified, "{entry:?}");
    client.pin(&id(server), true).await.unwrap();
}

/// A server with alice and bob, and a client of it.
async fn ready(test: &str) -> (TestServer, Client, Arc<MemoryStorage>) {
    let server = TestServer::start(test).await;
    server
        .add_user("alice", "correct horse", Some("Alice"))
        .await;
    server.add_user("bob", "battery staple", None).await;
    let storage = Arc::new(MemoryStorage::default());
    let client = client_on(&server, &storage).await;
    (server, client, storage)
}

fn id(server: &TestServer) -> String {
    server.server.id().to_string()
}

async fn alice(server: &TestServer, client: &Client) -> Session {
    client
        .sign_in(&id(server), "alice", "correct horse")
        .await
        .unwrap()
}

#[tokio::test]
async fn sign_in_and_browse() {
    let (server, client, _) = ready("browse").await;
    let alice = alice(&server, &client).await;
    assert_eq!(alice.user().display_name, "Alice");
    assert_eq!(alice.whoami().await.unwrap().username, "alice");

    assert_eq!(alice.libraries().await.unwrap(), [library()]);
    let page = alice.items(&library().id, 0, 50).await.unwrap();
    assert!(page.items.contains(&movie()));
    let detail = alice.item(show().id()).await.unwrap();
    assert_eq!(detail.children.len(), 3);
    assert_eq!(alice.search("ali", 10).await.unwrap(), [movie()]);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn wrong_password_is_invalid_credentials() {
    let (server, client, _) = ready("wrong-password").await;
    assert_eq!(
        client
            .sign_in(&id(&server), "alice", "nope")
            .await
            .unwrap_err(),
        SdkError::InvalidCredentials
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn two_users_share_one_connection_and_answer_as_themselves() {
    let (server, client, _) = ready("two-users").await;
    let alice = alice(&server, &client).await;
    let bob = client
        .sign_in(&id(&server), "bob", "battery staple")
        .await
        .unwrap();
    assert_eq!(alice.whoami().await.unwrap().username, "alice");
    assert_eq!(bob.whoami().await.unwrap().username, "bob");
    assert_eq!(server.server.connections().snapshot().len(), 1);

    let profiles = client.profiles(&id(&server)).await.unwrap();
    let mut names: Vec<_> = profiles.iter().map(|p| p.user().username.clone()).collect();
    names.sort();
    assert_eq!(names, ["alice", "bob"]);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn sessions_outlive_the_app() {
    let (server, client, storage) = ready("restart-app").await;
    alice(&server, &client).await;
    client.close().await;

    let again = client_on(&server, &storage).await;
    let profiles = again.profiles(&id(&server)).await.unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].whoami().await.unwrap().username, "alice");
    again.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_ended_session_is_forgotten_and_needs_a_sign_in() {
    let (server, client, storage) = ready("ended").await;
    let alice = alice(&server, &client).await;
    server.store.disable_user("alice").await.unwrap();

    assert_eq!(
        alice.libraries().await.unwrap_err(),
        SdkError::SignInRequired
    );
    assert!(client.profiles(&id(&server)).await.unwrap().is_empty());
    let key = format!("sessions/{}", id(&server));
    assert_eq!(storage.secret(&key).await.unwrap(), None);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_stale_profile_is_dropped_from_who_is_watching() {
    let (server, client, _) = ready("stale-profile").await;
    alice(&server, &client).await;
    client
        .sign_in(&id(&server), "bob", "battery staple")
        .await
        .unwrap();
    server.store.disable_user("bob").await.unwrap();
    let profiles = client.profiles(&id(&server)).await.unwrap();
    let names: Vec<_> = profiles.iter().map(|p| p.user().username.clone()).collect();
    assert_eq!(names, ["alice"]);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn sign_out_ends_the_session_here_and_there() {
    let (server, client, _) = ready("sign-out").await;
    let alice = alice(&server, &client).await;
    alice.sign_out().await.unwrap();
    assert!(client.profiles(&id(&server)).await.unwrap().is_empty());
    assert_eq!(alice.whoami().await.unwrap_err(), SdkError::SignInRequired);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_movie_plays_through_range_reads() {
    let (server, client, _) = ready("play").await;
    let file = server.write_media(3 << 20);
    let alice = alice(&server, &client).await;
    let playback = alice
        .start_playback(movie().id(), Some(Selection::default()))
        .await
        .unwrap();
    assert_eq!(playback.size, file.len() as u64);
    let mut read = Vec::new();
    while (read.len() as u64) < playback.size {
        let bytes = alice
            .read(&playback.handle, read.len() as u64, 1 << 20)
            .await
            .unwrap();
        assert!(!bytes.is_empty());
        read.extend(bytes);
    }
    assert_eq!(read, file);
    assert!(
        alice
            .read(&playback.handle, playback.size, 10)
            .await
            .unwrap()
            .is_empty()
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_episode_s_markers_come_with_its_details_and_its_playback() {
    let (server, first, _) = ready("markers").await;
    first.close().await;
    server.write_episode("15");
    // A device that plays the episode's file (h264) as it is.
    let mut capabilities = mkv_capabilities();
    capabilities.video.insert(VideoCodec::H264);
    let client = client_with(&server, &Arc::default(), capabilities).await;
    let alice = alice(&server, &client).await;
    let with_markers = episode("15", "14", 2, 1);
    assert_eq!(
        alice.item(with_markers.id()).await.unwrap().markers,
        markers()
    );
    let playback = alice.start_playback(with_markers.id(), None).await.unwrap();
    assert_eq!(playback.markers, markers());
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_item_without_markers_has_none_and_plays() {
    let (server, client, _) = ready("no-markers").await;
    server.write_media(1 << 10);
    let alice = alice(&server, &client).await;
    assert!(alice.item(movie().id()).await.unwrap().markers.is_empty());
    let playback = alice
        .start_playback(movie().id(), Some(Selection::default()))
        .await
        .unwrap();
    assert!(playback.markers.is_empty());
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_device_that_cannot_play_is_told_why() {
    let server = TestServer::start("cannot-play").await;
    server.write_media(1000);
    server.add_user("alice", "correct horse", None).await;
    let storage = Arc::new(MemoryStorage::default());
    let client = Client::open(storage, device(ClientCapabilities::default()))
        .await
        .unwrap();
    let addrs = server.server.addr().ip_addrs().copied().collect();
    client.add_address(&id(&server), addrs).unwrap();
    pin(&client, &server).await;
    let alice = alice(&server, &client).await;
    assert!(matches!(
        alice
            .start_playback(movie().id(), Some(Selection::default()))
            .await,
        Err(SdkError::CannotPlay { .. })
    ));
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn artwork_comes_at_the_asked_size() {
    let (server, client, _) = ready("artwork").await;
    let alice = alice(&server, &client).await;
    let bytes = alice.artwork(&poster(), ArtworkSize::Small).await.unwrap();
    assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 320);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn progress_shows_on_home_and_in_items() {
    let (server, client, _) = ready("progress").await;
    let alice = alice(&server, &client).await;
    alice
        .update_progress(movie().id(), Duration::from_secs(600))
        .await
        .unwrap();
    let home = alice.home().await.unwrap();
    assert_eq!(home.continue_watching.len(), 1);
    assert_eq!(home.continue_watching[0].id(), movie().id());

    let watched = episode("12", "11", 1, 1);
    alice.mark_watched(watched.id()).await.unwrap();
    let Item::Episode(e) = alice.item(watched.id()).await.unwrap().item else {
        panic!("not an episode");
    };
    assert!(matches!(e.playback, PlaybackState { watched: true, .. }));
    alice.mark_unwatched(watched.id()).await.unwrap();

    // "Remove from Continue watching", then "Undo" (P1-22).
    alice.hide_from_continue(movie().id()).await.unwrap();
    assert!(alice.home().await.unwrap().continue_watching.is_empty());
    alice.unhide_from_continue(movie().id()).await.unwrap();
    assert_eq!(alice.home().await.unwrap().continue_watching.len(), 1);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_unknown_server_is_refused_without_dialling() {
    let storage = Arc::new(MemoryStorage::default());
    let client = Client::open(storage, device(ClientCapabilities::default()))
        .await
        .unwrap();
    let stranger = iroh::SecretKey::generate().public().to_string();
    assert!(matches!(
        client.connect(&stranger).await,
        Err(SdkError::UnknownServer(_))
    ));
    client.close().await;
}

#[tokio::test]
async fn after_a_server_restart_the_same_sessions_work() {
    let (server, client, _) = ready("restart-server").await;
    let alice = alice(&server, &client).await;
    let mut state = client.state(&id(&server)).unwrap();
    assert_eq!(state.current(), ConnectionState::Connected);
    let seen = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(s) = state.changed().await {
            seen.push(s);
            if s == ConnectionState::Connected {
                break;
            }
        }
        seen
    });

    let server = server.restart().await;
    // Made while the client may still be reconnecting: it waits.
    assert_eq!(alice.libraries().await.unwrap(), [library()]);
    let seen = tokio::time::timeout(Duration::from_secs(10), seen)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(seen[0], ConnectionState::Reconnecting { attempt: 1 }),
        "{seen:?}"
    );
    assert_eq!(seen.last(), Some(&ConnectionState::Connected));
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_server_that_stays_away_is_unreachable_within_the_bound() {
    let (server, client, _) = ready("server-gone").await;
    let alice = alice(&server, &client).await;
    server.stop().await;
    let started = std::time::Instant::now();
    assert!(matches!(
        alice.libraries().await,
        Err(SdkError::Unreachable(_))
    ));
    // The wait for a reconnect, and a dial that may be under way.
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "{:?}",
        started.elapsed()
    );
    assert!(matches!(
        client.state(&alice.server()).unwrap().current(),
        ConnectionState::Reconnecting { .. }
    ));
    client.close().await;
}

#[tokio::test]
async fn track_preferences_round_trip_and_choose() {
    use spoolframe_client_sdk::TrackReason;
    use spoolframe_client_sdk::types::{MediaId, MediaKind, Namespace, SubtitleMode};

    let (server, client, _) = ready("track-preferences").await;
    server.write_episode("12");
    let alice = alice(&server, &client).await;
    let mut preferences = alice.track_preferences().await.unwrap();
    preferences.subtitles.mode = SubtitleMode::Off;
    alice.set_track_preferences(&preferences).await.unwrap();
    assert_eq!(alice.track_preferences().await.unwrap(), preferences);

    let episode = MediaId::new(Namespace::Plex, MediaKind::Episode, "12").unwrap();
    let plan = alice.playback_plan(&episode, None).await.unwrap();
    let chosen = plan.chosen.unwrap();
    assert_eq!(chosen.selection.audio, Some(2));
    assert_eq!(chosen.subtitle, TrackReason::NotWanted);
    client.close().await;
    server.stop().await;
}

/// A server converting a real file, and alice signed in on a device with
/// `capabilities`.
async fn converting(
    test: &str,
    capabilities: ClientCapabilities,
    ocr: bool,
) -> (TestServer, Client, Session) {
    let server = TestServer::start_with(
        support::fresh_dir(test).await,
        support::ServerOptions {
            real_media: true,
            ocr,
            ..support::ServerOptions::default()
        },
    )
    .await;
    server.generate_media(12, "libx264", "ac3");
    server.add_user("alice", "correct horse", None).await;
    let storage = Arc::new(MemoryStorage::default());
    let client = Client::open(storage, device(capabilities)).await.unwrap();
    let addrs = server.server.addr().ip_addrs().copied().collect();
    client.add_address(&id(&server), addrs).unwrap();
    pin(&client, &server).await;
    let alice = client
        .sign_in(&id(&server), "alice", "correct horse")
        .await
        .unwrap();
    (server, client, alice)
}

/// Every byte of `epoch`'s output, read as a player would.
async fn read_all(alice: &Session, handle: &PlaybackHandle, epoch: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let chunk = alice
            .read_chunk(handle, epoch, bytes.len() as u64, MAX_READ)
            .await
            .unwrap();
        assert_eq!(chunk.epoch, epoch);
        bytes.extend(chunk.bytes);
        if chunk.complete && bytes.len() as u64 == chunk.size {
            return bytes;
        }
    }
}

#[tokio::test]
async fn a_converted_playback_reads_to_complete_and_seeks() {
    let (server, client, alice) =
        converting("sdk-converted", support::progressive_capabilities(), false).await;
    let playback = alice
        .start_playback_with(movie().id(), None, PlaybackOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(playback.plan, PlaybackPlan::Converted { .. }),
        "{:?}",
        playback.plan
    );
    assert_eq!(playback.epoch, 0);
    let whole = read_all(&alice, &playback.handle, 0).await;
    assert!(whole.len() > 10_000, "{} bytes", whole.len());

    let epoch = alice
        .seek_playback(&playback.handle, Duration::from_secs(6))
        .await
        .unwrap();
    assert_eq!(epoch, 1);
    assert_eq!(
        alice.read_chunk(&playback.handle, 0, 0, 1024).await,
        Err(SdkError::PlaybackRestarted { epoch: 1 })
    );
    let rest = read_all(&alice, &playback.handle, 1).await;
    assert!(
        rest.len() < whole.len(),
        "{} of {}",
        rest.len(),
        whole.len()
    );

    alice.stop_playback(&playback.handle).await.unwrap();
    assert_eq!(
        alice.read_chunk(&playback.handle, 1, 0, 1024).await,
        Err(SdkError::PlaybackExpired)
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_hls_playback_gives_its_playlist_and_segments() {
    let (server, client, alice) = converting("sdk-hls", support::hls_capabilities(), false).await;
    let playback = alice
        .start_playback_with(movie().id(), None, PlaybackOptions::default())
        .await
        .unwrap();
    let media = alice
        .manifest(&playback.handle, ManifestKind::Media)
        .await
        .unwrap();
    assert!(
        media.contains("seg1.m4s") && media.contains("#EXT-X-ENDLIST"),
        "{media}"
    );
    for n in [0, 1] {
        let mut got = 0;
        loop {
            let chunk = alice
                .read_segment(&playback.handle, playback.epoch, n, got, MAX_READ)
                .await
                .unwrap();
            got += chunk.bytes.len() as u64;
            if chunk.complete && got == chunk.size {
                break;
            }
        }
        assert!(got > 0, "segment {n} is empty");
    }
    client.close().await;
    server.stop().await;
}

/// A subtitle too large for one frame (P2-11): the SDK reads it in chunks
/// and gives the caller the file.
#[tokio::test]
async fn a_subtitle_over_a_frame_arrives_whole() {
    let (server, client, _) = ready("sdk-large-subtitle").await;
    let alice = alice(&server, &client).await;
    server.write_media(1000);
    // No two chunks alike: 251 does not divide the chunk size. Three
    // chunks, the last a short one.
    let sidecar: Vec<u8> = (0..(1usize << 20) + 4321)
        .map(|i| (i % 251) as u8)
        .collect();
    std::fs::write(server.media.with_extension("en.srt"), &sidecar).unwrap();
    let tracks = alice.tracks(movie().id()).await.unwrap();
    let external = tracks.subtitles.iter().position(|s| s.external).unwrap() as u32;
    let selection = Selection {
        audio: None,
        subtitle: Some(external),
    };
    let playback = alice
        .start_playback(movie().id(), Some(selection))
        .await
        .unwrap();

    let subtitle = alice.read_subtitle(&playback.handle).await.unwrap();
    assert!(subtitle.bytes == sidecar, "{} bytes", subtitle.bytes.len());
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_pgs_subtitle_asked_for_as_text_arrives_as_text() {
    use spoolframe_client_sdk::types::{AudioCodec, Container, SubtitleFormat, VideoCodec};
    let capabilities = ClientCapabilities {
        video: [VideoCodec::H264].into(),
        audio: [AudioCodec::Ac3].into(),
        containers: [Container::Matroska].into(),
        subtitles: [SubtitleFormat::Srt, SubtitleFormat::Pgs].into(),
        ..ClientCapabilities::default()
    };
    let (server, client, alice) = converting("sdk-ocr", capabilities, true).await;
    server.add_pgs(&[("Hello there", 1_000, 3_500)]);
    let playback = alice
        .start_playback_with(
            movie().id(),
            Some(Selection {
                audio: Some(0),
                subtitle: Some(0),
            }),
            PlaybackOptions {
                bitmap: BitmapSubtitles::Text,
                ..PlaybackOptions::default()
            },
        )
        .await
        .unwrap();
    let subtitle = loop {
        match alice.read_subtitle(&playback.handle).await {
            Ok(subtitle) => break subtitle,
            Err(SdkError::SubtitleNotReady { .. }) => continue,
            Err(e) => panic!("{e}"),
        }
    };
    assert_eq!(subtitle.format, SubtitleFormat::Srt);
    assert_eq!(
        String::from_utf8(subtitle.bytes).unwrap(),
        "1\n00:00:01,000 --> 00:00:03,500\nHello there\n\n"
    );
    client.close().await;
    server.stop().await;
}

/// A profile with its PIN required on this device is locked when the app
/// opens again: "Who's watching" shows it so, its calls are refused, and
/// its PIN opens it (P2-08).
#[tokio::test]
async fn a_pin_required_here_locks_the_profile_when_the_app_opens_again() {
    let (server, client, storage) = ready("sdk-pin").await;
    let alice = alice(&server, &client).await;
    assert_eq!(
        alice.set_pin("12").await,
        Err(SdkError::InvalidInput("a PIN is 4 to 8 digits".into()))
    );
    alice.set_pin("4321").await.unwrap();
    alice.set_pin_required(None, true).await.unwrap();
    let devices = alice.devices().await.unwrap();
    assert!(
        devices.iter().any(|d| d.this_device && d.pin_required),
        "{devices:?}"
    );
    client.close().await;

    let client = client_on(&server, &storage).await;
    let profiles = client.profiles(&id(&server)).await.unwrap();
    let alice = profiles
        .into_iter()
        .find(|p| p.user().username == "alice")
        .unwrap();
    assert!(alice.user().locked);
    assert_eq!(alice.libraries().await, Err(SdkError::ProfileLocked));
    assert_eq!(
        alice.unlock("0000").await,
        Err(SdkError::InvalidCredentials)
    );
    alice.unlock("4321").await.unwrap();
    alice.libraries().await.unwrap();
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_tv_is_signed_in_from_a_phone_by_its_qr_code() {
    let (server, phone, _) = ready("sdk-device-sign-in").await;
    let alice = alice(&server, &phone).await;
    let tv_storage = Arc::new(MemoryStorage::default());
    let tv = client_on(&server, &tv_storage).await;

    let sign_in = tv.begin_device_sign_in(&id(&server)).await.unwrap();
    assert_eq!(
        sign_in.display_code,
        format!("{}-{}", &sign_in.code[..4], &sign_in.code[4..])
    );
    // This server's web UI has no public URL: no page to point a camera at.
    assert_eq!(sign_in.web_url, None);
    let waiting = {
        let tv = tv.clone();
        let sign_in = sign_in.clone();
        tokio::spawn(async move { tv.await_device_sign_in(&sign_in).await })
    };
    // The phone scans the QR code: which server, and which code.
    let scanned = spoolframe_client_sdk::device_sign_in::parse_link(&sign_in.link).unwrap();
    assert_eq!(scanned.server, id(&server));
    let device = alice.lookup_device_sign_in(&scanned.code).await.unwrap();
    assert_eq!(device.device_name.as_deref(), Some("living room"));
    alice.approve_device_sign_in(&scanned.code).await.unwrap();

    let on_tv = waiting.await.unwrap().unwrap();
    assert_eq!(on_tv.user().username, "alice");
    on_tv.libraries().await.unwrap();
    // Kept, as a password sign-in is: the TV's profiles list it.
    let profiles = tv.profiles(&id(&server)).await.unwrap();
    assert_eq!(profiles.len(), 1);

    // A second one, refused.
    let sign_in = tv.begin_device_sign_in(&id(&server)).await.unwrap();
    alice.deny_device_sign_in(&sign_in.code).await.unwrap();
    assert_eq!(
        tv.await_device_sign_in(&sign_in).await.unwrap_err(),
        SdkError::SignInDenied
    );
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

/// The app's QR code for a phone without the app (P3-05): the web UI's
/// approval page, opened in the phone's browser, signed in to there.
#[tokio::test]
async fn a_tv_is_signed_in_from_a_browser_by_its_web_qr_code() {
    use spoolframe_admin_api::MeApiClient;
    const WEB: &str = "https://media.example.org";
    let options = ServerOptions {
        public_url: Some(WEB.into()),
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(fresh_dir("sdk-device-sign-in-web").await, options).await;
    server
        .add_user("alice", "correct horse", Some("Alice"))
        .await;
    let web = crate::admin::serve_beside(server.auth.clone(), WEB).await;
    let tv = client_on(&server, &Arc::new(MemoryStorage::default())).await;

    let sign_in = tv.begin_device_sign_in(&id(&server)).await.unwrap();
    let page = sign_in.web_url.clone().expect("a page to approve on");
    assert_eq!(page, format!("{WEB}/auth/device?code={}", sign_in.code));
    let waiting = {
        let tv = tv.clone();
        let sign_in = sign_in.clone();
        tokio::spawn(async move { tv.await_device_sign_in(&sign_in).await })
    };
    // The phone's camera opens the page; its user signs in and approves
    // the code the page was opened with.
    let code = page.split_once("?code=").unwrap().1.to_owned();
    let login = crate::admin::anonymous(web)
        .await
        .login(crate::admin::creds("alice", "correct horse"))
        .await
        .unwrap();
    let alice = crate::admin::with_token(web, &login.token).await;
    let device = alice.device_sign_in_lookup(code.clone()).await.unwrap();
    assert_eq!(device.device_name.as_deref(), Some("living room"));
    alice.device_sign_in_approve(code).await.unwrap();

    let on_tv = waiting.await.unwrap().unwrap();
    assert_eq!(on_tv.user().username, "alice");
    on_tv.libraries().await.unwrap();
    tv.close().await;
    server.stop().await;
}
