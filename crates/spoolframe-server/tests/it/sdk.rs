//! The client SDK (P1-15) against a real server in-process. The server is
//! off the LAN here; the SDK is given its address, as a typed-in address
//! would be. Discovery is tested in `lan.rs`.

use crate::support;

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Item, PlaybackState, VideoCodec};
use spoolframe_client_sdk::{
    ArtworkSize, BitmapSubtitles, Client, ConnectionPath, ConnectionState, Device, MAX_READ,
    ManifestKind, MemoryStorage, PinProof, PlaybackHandle, PlaybackOptions, PlaybackPlan,
    ProfileSignIn, SdkError, Selection, Session, SignInMethod, Storage, TopResult,
};
use spoolframe_server::admission::Admission;
use support::{
    ServerOptions, TestServer, episode, fresh_dir, library, markers, mkv_capabilities, movie,
    poster, show,
};

fn device(capabilities: ClientCapabilities) -> Device {
    Device {
        name: "living room".into(),
        client_name: "sdk-test".into(),
        client_version: "0".into(),
        platform: spoolframe_client_sdk::types::Platform::Tvos,
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
        .sign_in(&id(server), "alice", "correct horse", false)
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
    assert_eq!(
        alice.search(support::search("ali")).await.unwrap().top,
        Some(TopResult::Item(movie()))
    );
    client.close().await;
    server.stop().await;
}

/// `session`'s connection path once a direct path is selected, or whatever it is
/// after `within`: right after a sign-in iroh may not have selected a
/// path yet.
async fn settled_path(session: &Session, within: Duration) -> ConnectionPath {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let path = session.connection_path();
        if matches!(
            path,
            ConnectionPath::Lan { .. } | ConnectionPath::Direct { .. }
        ) || tokio::time::Instant::now() >= deadline
        {
            return path;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// An address only this network reaches: what `ConnectionPath::Lan` is.
fn on_the_lan(addr: &std::net::SocketAddr) -> bool {
    match addr.ip() {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_loopback(),
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local(),
    }
}

#[tokio::test]
async fn a_session_tells_its_direct_path_as_lan_or_not() {
    let server = TestServer::start("path").await;
    server
        .add_user("alice", "correct horse", Some("Alice"))
        .await;
    // Only the server's LAN addresses. Given its public IPv6 address as
    // well, iroh may take that path first, and keeps it: a path as fast
    // as this one is good enough, and it looks for a better one only
    // once a minute.
    let lan: Vec<_> = server
        .server
        .addr()
        .ip_addrs()
        .copied()
        .filter(on_the_lan)
        .collect();
    assert!(!lan.is_empty(), "the server has no LAN address");
    let storage = Arc::new(MemoryStorage::default());
    let client = Client::open(storage, device(mkv_capabilities()))
        .await
        .unwrap();
    client.add_address(&id(&server), lan).unwrap();
    pin(&client, &server).await;
    let alice = alice(&server, &client).await;

    // iroh learns the server's other addresses during the handshake and
    // may still move to a public one, so which path it settles on is not
    // ours to fix: what is checked is that the path is told as what it is,
    // a LAN address as Lan and any other as Direct.
    let path = settled_path(&alice, Duration::from_secs(5)).await;
    let addr = match path {
        ConnectionPath::Lan { addr } => {
            assert!(on_the_lan(&addr), "{addr} is told as LAN but is not");
            addr
        }
        ConnectionPath::Direct { addr } => {
            assert!(
                !on_the_lan(&addr),
                "{addr} is on the LAN but told as Direct"
            );
            addr
        }
        other => panic!("no direct path within 5 s: {other:?}"),
    };
    assert!(
        server.server.addr().ip_addrs().any(|a| *a == addr),
        "{addr} is not one of the server's"
    );
    // The same, asked of the client before a profile is chosen.
    assert_eq!(
        client.connection_path(&id(&server)).unwrap(),
        alice.connection_path()
    );
    assert!(client.connection_path("not an endpoint").is_err());
    client.close().await;
    assert_eq!(alice.connection_path(), ConnectionPath::Unknown);
    assert_eq!(
        client.connection_path(&id(&server)).unwrap(),
        ConnectionPath::Unknown
    );
    server.stop().await;
}

#[tokio::test]
async fn wrong_password_is_invalid_credentials() {
    let (server, client, _) = ready("wrong-password").await;
    assert_eq!(
        client
            .sign_in(&id(&server), "alice", "nope", false)
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
        .sign_in(&id(&server), "bob", "battery staple", false)
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

/// Who's watching as this device keeps it: each name and how they sign
/// in.
async fn saved(client: &Client, server: &TestServer) -> Vec<(String, ProfileSignIn)> {
    client
        .saved_profiles(&id(server))
        .await
        .unwrap()
        .into_iter()
        .map(|p| (p.username, p.sign_in))
        .collect()
}

/// How each profile this device keeps last signed in.
async fn methods(client: &Client, server: &TestServer) -> Vec<(String, Option<SignInMethod>)> {
    client
        .saved_profiles(&id(server))
        .await
        .unwrap()
        .into_iter()
        .map(|p| (p.username, p.method))
        .collect()
}

/// A password sign-in is remembered past the end of its session and the
/// app: "Who's watching" can still say how alice signs in.
#[tokio::test]
async fn an_ended_profile_remembers_it_signed_in_with_a_password() {
    let (server, client, storage) = ready("method-password").await;
    alice(&server, &client).await;
    server.store.disable_user("alice").await.unwrap();
    assert!(client.profiles(&id(&server)).await.unwrap().is_empty());
    assert_eq!(
        saved(&client, &server).await,
        [("alice".to_owned(), ProfileSignIn::Password)]
    );
    assert_eq!(
        methods(&client, &server).await,
        [("alice".to_owned(), Some(SignInMethod::Password))]
    );
    client.close().await;

    let again = Client::open(storage.clone(), device(mkv_capabilities()))
        .await
        .unwrap();
    assert_eq!(
        methods(&again, &server).await,
        [("alice".to_owned(), Some(SignInMethod::Password))]
    );
    again.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_ended_session_keeps_its_profile_which_needs_a_sign_in() {
    let (server, client, storage) = ready("ended").await;
    let alice = alice(&server, &client).await;
    assert_eq!(
        saved(&client, &server).await,
        [("alice".to_owned(), ProfileSignIn::Automatic)]
    );
    server.store.disable_user("alice").await.unwrap();

    assert_eq!(
        alice.libraries().await.unwrap_err(),
        SdkError::SignInRequired
    );
    assert!(client.profiles(&id(&server)).await.unwrap().is_empty());
    assert_eq!(
        saved(&client, &server).await,
        [("alice".to_owned(), ProfileSignIn::Password)]
    );
    // The token is gone from storage; only the names are kept.
    let key = format!("sessions/{}", id(&server));
    let kept: serde_json::Value =
        serde_json::from_slice(&storage.secret(&key).await.unwrap().unwrap()).unwrap();
    assert_eq!(kept[0]["token"], serde_json::Value::Null, "{kept}");
    assert_eq!(kept[0]["display_name"], "Alice", "{kept}");
    client.close().await;
    server.stop().await;
}

/// "Who's watching" read the sessions kept just before the server signed
/// the device out (PX-10), and asks after one only once the sign-out has
/// ended it: that profile needs a password, as any ended one does, and the
/// list is not an error. Putting the token back after the sign-out makes
/// that interleaving certain.
#[tokio::test]
async fn profiles_read_across_a_sign_out_list_it_as_ended() {
    let (server, client, storage) = ready("profiles-across-sign-out").await;
    alice(&server, &client).await;
    let key = format!("sessions/{}", id(&server));
    let before = storage.secret(&key).await.unwrap();
    let mut signed_out = client.signed_out(&id(&server)).unwrap();
    server.store.disable_user("alice").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), signed_out.next())
        .await
        .expect("no sign-out within 5 s")
        .expect("the client is open");
    storage.set_secret(&key, before).await.unwrap();

    assert!(client.profiles(&id(&server)).await.unwrap().is_empty());
    assert_eq!(
        saved(&client, &server).await,
        [("alice".to_owned(), ProfileSignIn::Password)]
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn signing_in_again_makes_an_ended_profile_automatic() {
    let (server, client, _) = ready("ended-again").await;
    alice(&server, &client).await;
    server.store.disable_user("alice").await.unwrap();
    assert!(client.profiles(&id(&server)).await.unwrap().is_empty());
    server.store.enable_user("alice").await.unwrap();
    alice(&server, &client).await;
    assert_eq!(
        saved(&client, &server).await,
        [("alice".to_owned(), ProfileSignIn::Automatic)]
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_stale_profile_is_kept_for_a_password_and_out_of_the_sessions() {
    let (server, client, _) = ready("stale-profile").await;
    alice(&server, &client).await;
    client
        .sign_in(&id(&server), "bob", "battery staple", false)
        .await
        .unwrap();
    server.store.disable_user("bob").await.unwrap();
    let profiles = client.profiles(&id(&server)).await.unwrap();
    let names: Vec<_> = profiles.iter().map(|p| p.user().username.clone()).collect();
    assert_eq!(names, ["alice"]);
    assert_eq!(
        saved(&client, &server).await,
        [
            ("alice".to_owned(), ProfileSignIn::Automatic),
            ("bob".to_owned(), ProfileSignIn::Password),
        ]
    );
    client.forget_profile(&id(&server), "bob").await.unwrap();
    assert_eq!(
        saved(&client, &server).await,
        [("alice".to_owned(), ProfileSignIn::Automatic)]
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn saved_profiles_are_read_without_the_server() {
    let (server, client, storage) = ready("saved-offline").await;
    let key = id(&server);
    alice(&server, &client).await;
    client.close().await;
    server.stop().await;
    let again = Client::open(storage.clone(), device(mkv_capabilities()))
        .await
        .unwrap();
    let profiles = again.saved_profiles(&key).await.unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].display_name, "Alice");
    assert_eq!(profiles[0].sign_in, ProfileSignIn::Automatic);
    again.close().await;
}

#[tokio::test]
async fn sign_out_ends_the_session_here_and_there() {
    let (server, client, _) = ready("sign-out").await;
    let alice = alice(&server, &client).await;
    alice.sign_out().await.unwrap();
    assert!(client.profiles(&id(&server)).await.unwrap().is_empty());
    // Signing out is leaving: the profile goes too.
    assert!(saved(&client, &server).await.is_empty());
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
    server.write_media(1000);
    let playback = alice
        .start_playback(movie().id(), Some(Default::default()))
        .await
        .unwrap();
    alice
        .update_progress(&playback.handle, Duration::from_secs(600))
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
        .sign_in(&id(&server), "alice", "correct horse", false)
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
        alice.set_pin("12", None).await,
        Err(SdkError::InvalidInput("a PIN is 4 to 8 digits".into()))
    );
    alice.set_pin("4321", None).await.unwrap();
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
    assert_eq!(
        alice.user().pin,
        Some(spoolframe_client_sdk::PinState { digits: Some(4) })
    );
    // Change PIN and Remove PIN take the current PIN, or the password.
    let pin = |pin: &str| PinProof::Pin { pin: pin.into() };
    let password = |password: &str| PinProof::Password {
        password: password.into(),
    };
    assert_eq!(
        alice.set_pin("98765", Some(pin("0000"))).await,
        Err(SdkError::InvalidCredentials)
    );
    assert_eq!(
        alice
            .set_pin("98765", Some(password("wrong password")))
            .await,
        Err(SdkError::InvalidCredentials)
    );
    alice.set_pin("98765", Some(pin("4321"))).await.unwrap();
    alice
        .set_pin("2468", Some(password("correct horse")))
        .await
        .unwrap();
    alice.clear_pin(pin("2468")).await.unwrap();
    alice.set_pin("4321", None).await.unwrap();
    alice.clear_pin(password("correct horse")).await.unwrap();
    assert_eq!(alice.whoami().await.unwrap().pin, None);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_tv_is_signed_in_from_a_phone_by_its_qr_code() {
    let (server, phone, _) = ready("sdk-device-sign-in").await;
    let alice = alice(&server, &phone).await;
    let tv_storage = Arc::new(MemoryStorage::default());
    let tv = client_on(&server, &tv_storage).await;

    let sign_in = tv.begin_device_sign_in(&id(&server), false).await.unwrap();
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
    alice
        .approve_device_sign_in(&scanned.code, false)
        .await
        .unwrap();

    let on_tv = waiting.await.unwrap().unwrap();
    assert_eq!(on_tv.user().username, "alice");
    on_tv.libraries().await.unwrap();
    // Kept, as a password sign-in is: the TV's profiles list it.
    let profiles = tv.profiles(&id(&server)).await.unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(
        methods(&tv, &server).await,
        [("alice".to_owned(), Some(SignInMethod::Device))]
    );

    // A second one, refused.
    let sign_in = tv.begin_device_sign_in(&id(&server), false).await.unwrap();
    alice.deny_device_sign_in(&sign_in.code).await.unwrap();
    assert_eq!(
        tv.await_device_sign_in(&sign_in).await.unwrap_err(),
        SdkError::SignInDenied
    );
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

/// Where the server's web UI is, from its Hello, for links into it such as
/// linking a Plex account at `<web_url>/profile`.
#[tokio::test]
async fn a_session_knows_where_the_server_s_web_ui_is() {
    const WEB: &str = "https://media.example.org";
    let options = ServerOptions {
        public_url: Some(WEB.into()),
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(fresh_dir("sdk-web-url").await, options).await;
    server.add_user("alice", "correct horse", None).await;
    let client = client_on(&server, &Arc::new(MemoryStorage::default())).await;
    assert_eq!(
        alice(&server, &client).await.web_url().as_deref(),
        Some(WEB)
    );
    client.close().await;
    server.stop().await;

    // No public URL: no link, rather than an address no browser trusts.
    let (server, client, _) = ready("sdk-no-web-url").await;
    assert_eq!(alice(&server, &client).await.web_url(), None);
    client.close().await;
    server.stop().await;
}

/// A player scrubbing (P3-13): the previews come once they are made, and
/// each sheet is an image like any other.
#[tokio::test]
async fn a_player_finds_the_frame_for_a_time_in_an_item_s_previews() {
    use spoolframe_server::previews::{PreviewSettings, When};
    let options = ServerOptions {
        real_media: true,
        previews: PreviewSettings {
            when: When::OnPlay,
            interval: Duration::from_secs(10),
            cache_limit_bytes: 1 << 30,
            workers: 1,
        },
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(fresh_dir("sdk-previews").await, options).await;
    server.generate_media(30, "libx264", "ac3");
    server.add_user("alice", "correct horse", None).await;
    let client = client_on(&server, &Arc::new(MemoryStorage::default())).await;
    let session = alice(&server, &client).await;
    let previews = loop {
        match session.previews(movie().id()).await {
            Ok(previews) => break previews,
            Err(SdkError::PreviewsNotReady { .. }) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("{e}"),
        }
    };
    assert_eq!(previews.count, 3);
    let tile = previews.tile(25_000).unwrap();
    assert_eq!((tile.index, tile.sheet, tile.x, tile.y), (2, 0, 640, 0));
    let sheet = session
        .artwork(&previews.sheets[tile.sheet], ArtworkSize::Original)
        .await
        .unwrap();
    let sheet = image::load_from_memory(&sheet).unwrap();
    assert_eq!(sheet.width(), previews.columns * previews.tile_width);
    assert_eq!(sheet.height(), previews.rows * previews.tile_height);
    client.close().await;
    server.stop().await;

    // A server that makes none.
    let (server, client, _) = ready("sdk-no-previews").await;
    server.write_media(1000);
    let session = alice(&server, &client).await;
    assert_eq!(
        session.previews(movie().id()).await.unwrap_err(),
        SdkError::NoPreviews
    );
    client.close().await;
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

    let sign_in = tv.begin_device_sign_in(&id(&server), false).await.unwrap();
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
    alice.device_sign_in_approve(code, false).await.unwrap();

    let on_tv = waiting.await.unwrap().unwrap();
    assert_eq!(on_tv.user().username, "alice");
    on_tv.libraries().await.unwrap();
    tv.close().await;
    server.stop().await;
}

/// Preferences' Plex row: whether this user's Plex account is linked, and
/// as whom; linking itself is in the web UI (P3-12).
#[tokio::test]
async fn a_session_reads_its_user_s_plex_link() {
    let (server, client, _) = ready("sdk-plex-link").await;
    let alice = alice(&server, &client).await;
    assert_eq!(alice.plex_link().await.unwrap(), None);

    let alice_id = server
        .store
        .password_login("alice")
        .await
        .unwrap()
        .unwrap()
        .id;
    server
        .plex_accounts
        .save(
            alice_id,
            &spoolframe_server::plex_accounts::AccountLink {
                token: "plex-token".into(),
                username: Some("alice_plex".into()),
                email: Some("alice@example.com".into()),
                linked_at: 1_790_000_000,
            },
        )
        .unwrap();
    let link = alice.plex_link().await.unwrap().expect("linked");
    assert_eq!(link.username.as_deref(), Some("alice_plex"));
    assert_eq!(link.email.as_deref(), Some("alice@example.com"));
    assert_eq!(link.linked_at, Some(1_790_000_000));
    assert!(!link.by_config);

    // Only hers: bob's session says he is not linked.
    let bob = client
        .sign_in(&id(&server), "bob", "battery staple", false)
        .await
        .unwrap();
    assert_eq!(bob.plex_link().await.unwrap(), None);
    client.close().await;
    server.stop().await;
}

fn secs(at: Option<std::time::SystemTime>) -> u64 {
    at.expect("dated")
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Preferences' Servers: when each server's fingerprint was checked and
/// when it last answered, kept with the saved servers.
#[tokio::test]
async fn a_saved_server_is_dated_when_pinned_verified_and_connected() {
    let server = TestServer::start("sdk-server-dates").await;
    let storage = Arc::new(MemoryStorage::default());
    let before = std::time::SystemTime::now();
    let client = client_on(&server, &storage).await;
    let entry = client.servers().await.unwrap().remove(0);
    assert!(entry.pinned && entry.verified);
    for at in [entry.pinned_at, entry.verified_at, entry.last_connected_at] {
        assert!(
            at.is_some_and(|at| at >= before - Duration::from_secs(1)),
            "{entry:?}"
        );
    }
    client.close().await;

    // Long ago, as far as the store knows.
    let mut saved: serde_json::Value =
        serde_json::from_slice(&storage.data("servers").await.unwrap().unwrap()).unwrap();
    saved[0]["pinned_at"] = 1.into();
    saved[0]["verified_at"] = 2.into();
    saved[0]["last_connected_at"] = 3.into();
    storage
        .set_data("servers", Some(serde_json::to_vec(&saved).unwrap()))
        .await
        .unwrap();

    // The app opens again and connects: only the connect is dated anew.
    let client = Client::open(storage.clone(), device(mkv_capabilities()))
        .await
        .unwrap();
    let addrs = server.server.addr().ip_addrs().copied().collect();
    client.add_address(&id(&server), addrs).unwrap();
    let connected = client.connect(&id(&server)).await.unwrap();
    assert!(secs(connected.last_connected_at) > 3);
    assert_eq!(
        (secs(connected.pinned_at), secs(connected.verified_at)),
        (1, 2)
    );
    let listed = client.servers().await.unwrap().remove(0);
    assert_eq!(listed.last_connected_at, connected.last_connected_at);

    // Not verified after all: no verification date.
    let unverified = client.pin(&id(&server), false).await.unwrap();
    assert_eq!(unverified.verified_at, None);
    assert_eq!(secs(unverified.pinned_at), 1);
    client.close().await;
    server.stop().await;
}

/// An app passkey (P3-19) made on the phone through the SDK signs the TV
/// in, and the TV keeps the session as a password sign-in's.
#[tokio::test]
async fn an_app_passkey_made_on_the_phone_signs_the_tv_in() {
    let (server, phone, _) = ready("app-passkey").await;
    let alice = alice(&server, &phone).await;
    let creation = alice.begin_passkey_registration().await.unwrap();
    assert_eq!(creation.user_name, "alice on Home Media");
    let mut app = crate::app_passkeys::App::new();
    alice
        .finish_passkey_registration(app.create(&creation, "Alice's iPhone"))
        .await
        .unwrap();

    let tv_storage = Arc::new(MemoryStorage::default());
    let tv = client_on(&server, &tv_storage).await;
    let request = tv.begin_passkey_sign_in(&id(&server), None).await.unwrap();
    let signed = app.get(&request);
    let session = tv
        .finish_passkey_sign_in(&id(&server), signed.clone(), false)
        .await
        .unwrap();
    assert_eq!(session.user().username, "alice");
    let profiles = tv.profiles(&id(&server)).await.unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].user().username, "alice");
    // Once.
    assert_eq!(
        tv.finish_passkey_sign_in(&id(&server), signed, false)
            .await
            .unwrap_err(),
        SdkError::InvalidCredentials
    );
    // Once the session ends, the TV still knows to offer the passkey.
    server.store.disable_user("alice").await.unwrap();
    assert!(tv.profiles(&id(&server)).await.unwrap().is_empty());
    assert_eq!(
        saved(&tv, &server).await,
        [("alice".to_owned(), ProfileSignIn::Password)]
    );
    assert_eq!(
        methods(&tv, &server).await,
        [("alice".to_owned(), Some(SignInMethod::Passkey))]
    );
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

/// The app lists, renames and removes its user's passkeys through the SDK
/// (P3-19), the web UI's with its own, under the web UI's rules.
#[tokio::test]
async fn the_app_manages_its_user_s_passkeys() {
    use crate::app_passkeys::{App, app_one, sql, web_one, web_passkey};

    let (server, phone, _) = ready("app-passkey-manage").await;
    let alice = alice(&server, &phone).await;
    let creation = alice.begin_passkey_registration().await.unwrap();
    let mut app = App::new();
    alice
        .finish_passkey_registration(app.create(&creation, "Alice's iPhone"))
        .await
        .unwrap();
    web_passkey(&server, "alice", "d2ViLTE", "YubiKey").await;

    let listed = alice.passkeys().await.unwrap();
    assert_eq!(listed.len(), 2);
    let flags = |p: &spoolframe_client_sdk::PasskeyInfo| (p.name.clone(), p.app, p.usable_here);
    assert_eq!(
        flags(app_one(&listed)),
        ("Alice's iPhone".to_owned(), true, true)
    );
    assert_eq!(
        flags(web_one(&listed)),
        ("YubiKey".to_owned(), false, false)
    );
    let (made, web) = (app_one(&listed).id.clone(), web_one(&listed).id.clone());

    alice.rename_passkey(&made, "Kitchen iPad").await.unwrap();
    let listed = alice.passkeys().await.unwrap();
    assert_eq!(app_one(&listed).name, "Kitchen iPad");
    assert!(matches!(
        alice.rename_passkey(&made, "").await,
        Err(SdkError::InvalidInput(_))
    ));
    assert!(matches!(
        alice.remove_passkey("bm9wZQ").await,
        Err(SdkError::NotFound(_))
    ));

    alice.remove_passkey(&web).await.unwrap();
    assert_eq!(alice.passkeys().await.unwrap().len(), 1);
    // With no password, the one left is her way in.
    sql(
        &server,
        "DELETE FROM password_credentials WHERE user_id = \
         (SELECT id FROM users WHERE username = ?1)",
        "alice",
    )
    .await;
    assert_eq!(
        alice.remove_passkey(&made).await,
        Err(SdkError::LastSignInMethod)
    );
    // Signed in an hour ago, she signs in again first.
    sql(
        &server,
        "UPDATE sessions SET authenticated_at = authenticated_at - 3600 WHERE user_id = \
         (SELECT id FROM users WHERE username = ?1)",
        "alice",
    )
    .await;
    assert_eq!(
        alice.remove_passkey(&made).await,
        Err(SdkError::RecentSignInRequired)
    );
    assert_eq!(alice.passkeys().await.unwrap().len(), 1);
    phone.close().await;
    server.stop().await;
}

/// An app signed in long ago confirms it is its user (P3-07), with the
/// password or an app passkey, and may then approve a TV; a wrong
/// password confirms nothing.
#[tokio::test]
async fn an_old_session_confirms_it_is_its_user_before_approving_a_tv() {
    let (server, phone, _) = ready("sdk-confirm").await;
    let alice = alice(&server, &phone).await;
    let creation = alice.begin_passkey_registration().await.unwrap();
    let mut app = crate::app_passkeys::App::new();
    alice
        .finish_passkey_registration(app.create(&creation, "Alice's iPhone"))
        .await
        .unwrap();
    let tv_storage = Arc::new(MemoryStorage::default());
    let tv = client_on(&server, &tv_storage).await;
    let aged = || {
        crate::app_passkeys::sql(
            &server,
            "UPDATE sessions SET authenticated_at = authenticated_at - 3600 WHERE user_id = \
             (SELECT id FROM users WHERE username = ?1)",
            "alice",
        )
    };

    aged().await;
    let sign_in = tv.begin_device_sign_in(&id(&server), false).await.unwrap();
    assert_eq!(
        alice.approve_device_sign_in(&sign_in.code, false).await,
        Err(SdkError::RecentSignInRequired)
    );
    assert_eq!(
        alice.confirm_password("battery staple").await,
        Err(SdkError::InvalidCredentials)
    );
    alice.confirm_password("correct horse").await.unwrap();
    alice
        .approve_device_sign_in(&sign_in.code, false)
        .await
        .unwrap();

    aged().await;
    let sign_in = tv.begin_device_sign_in(&id(&server), false).await.unwrap();
    assert_eq!(
        alice.approve_device_sign_in(&sign_in.code, false).await,
        Err(SdkError::RecentSignInRequired)
    );
    let request = alice.begin_confirm_passkey().await.unwrap();
    alice
        .finish_confirm_passkey(app.get(&request))
        .await
        .unwrap();
    alice
        .approve_device_sign_in(&sign_in.code, false)
        .await
        .unwrap();
    // A challenge is used once.
    assert_eq!(
        alice.finish_confirm_passkey(app.get(&request)).await,
        Err(SdkError::InvalidCredentials)
    );
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn unchanged_sdk_calls_reassemble_large_replies_and_serialize_controls() {
    let (server, client, _) = ready("sdk-wire-revision").await;
    server.write_media(1000);
    let alice = alice(&server, &client).await;
    let large = alice
        .search(support::search(support::A_TITLE_OVER_A_FRAME))
        .await
        .unwrap();
    assert!(
        spoolframe_protocol::codec::encode(&large).unwrap().len()
            > spoolframe_protocol::codec::MAX_FRAME as usize
    );
    let playback = alice
        .start_playback(movie().id(), Some(Selection::default()))
        .await
        .unwrap();
    let other = alice.clone();
    let (one, two) = tokio::join!(
        alice.change_subtitle(&playback.handle, None, Default::default(), Duration::ZERO),
        other.change_subtitle(&playback.handle, None, Default::default(), Duration::ZERO),
    );
    one.unwrap();
    two.unwrap();
    assert_eq!(alice.read(&playback.handle, 0, 10).await.unwrap().len(), 10);
    alice.stop_playback(&playback.handle).await.unwrap();
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn selecting_a_profile_is_explicit_and_background_reads_do_not_switch_it() {
    let (server, client, _) = ready("sdk-explicit-profile").await;
    let alice = alice(&server, &client).await;
    let bob = client
        .sign_in(&id(&server), "bob", "battery staple", false)
        .await
        .unwrap();
    let active = client.activate_profile(&alice).await.unwrap();
    active.set_pin("4321", None).await.unwrap();
    active.set_pin_required(None, true).await.unwrap();
    bob.libraries().await.unwrap();
    assert!(!active.whoami().await.unwrap().locked);
    active.home().await.unwrap();
    client.activate_profile(&bob).await.unwrap();
    assert!(matches!(active.home().await, Err(SdkError::ProfileLocked)));
    let active = client.activate_profile(&alice).await.unwrap();
    active.unlock("4321").await.unwrap();
    active.home().await.unwrap();
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn progress_sequences_resume_when_the_sdk_reopens_with_a_saved_handle() {
    let (server, client, storage) = ready("sdk-progress-resume").await;
    server.write_media(1000);
    let alice = alice(&server, &client).await;
    let playback = alice
        .start_playback(movie().id(), Some(Default::default()))
        .await
        .unwrap();
    alice
        .update_progress(&playback.handle, Duration::from_secs(30))
        .await
        .unwrap();
    client.close().await;
    let client = client_on(&server, &storage).await;
    let alice = client.profiles(&id(&server)).await.unwrap().remove(0);
    alice
        .update_progress(&playback.handle, Duration::from_secs(600))
        .await
        .unwrap();
    alice.stop_playback(&playback.handle).await.unwrap();
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn managed_playback_recovers_then_reports_and_stops_the_replacement() {
    let (server, client, _) = ready("sdk-managed-playback").await;
    let file = server.write_media(1000);
    let alice = alice(&server, &client).await;
    let playback = alice
        .open_playback(
            movie().id(),
            Some(Selection::default()),
            PlaybackOptions::default(),
        )
        .await
        .unwrap();
    let old = playback.snapshot().handle;
    // Force the same missing-handle response as idle expiry, without waiting.
    alice.stop_playback(&old).await.unwrap();
    assert_eq!(playback.read(0, 1000).await.unwrap(), file);
    let new = playback.snapshot().handle;
    assert_ne!(old, new);
    playback.report_position(Duration::from_secs(60)).unwrap();
    playback.finish(Duration::from_secs(600)).await.unwrap();
    let home = alice.home().await.unwrap();
    assert_eq!(home.continue_watching.len(), 1);
    let Item::Movie(item) = &home.continue_watching[0] else {
        panic!("expected movie")
    };
    assert_eq!(item.playback.position, Some(Duration::from_secs(600)));
    assert_eq!(alice.read(&new, 0, 1).await, Err(SdkError::PlaybackExpired));
    client.close().await;
    server.stop().await;
}

/// A server that holds at most one device with no session (PX-10), with
/// alice, and a TV signed in as her. The TV is let in past the cap: it is
/// signed in. A stranger takes the one place left.
async fn full_of_strangers(test: &str) -> (TestServer, Client, Session, support::TestClient) {
    let server = TestServer::start_with(
        fresh_dir(test).await,
        ServerOptions {
            admission: Admission {
                max_strangers: 1,
                ..Admission::default()
            },
            ..ServerOptions::default()
        },
    )
    .await;
    server
        .add_user("alice", "correct horse", Some("Alice"))
        .await;
    let storage = Arc::new(MemoryStorage::default());
    let tv = client_on(&server, &storage).await;
    let alice = alice(&server, &tv).await;
    let stranger = server.client().await;
    stranger.hello().await;
    (server, tv, alice, stranger)
}

/// The server closes the connection of a device whose last session ended
/// (PX-10). The SDK tells the app at once, with no request in flight, and
/// forgets the device's sessions before it does.
#[tokio::test]
async fn a_device_signed_out_on_the_server_is_told_with_no_request_in_flight() {
    let (server, tv, _alice, _stranger) = full_of_strangers("sdk-signed-out-watch").await;
    let mut signed_out = tv.signed_out(&id(&server)).unwrap();
    server.store.disable_user("alice").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), signed_out.next())
        .await
        .expect("no sign-out within 5 s")
        .expect("the client is open");
    assert_eq!(
        saved(&tv, &server).await,
        [("alice".to_owned(), ProfileSignIn::Password)]
    );
    // It does not dial again as a device with no session.
    assert_eq!(
        tv.state(&id(&server)).unwrap().current(),
        ConnectionState::Disconnected
    );
    tv.close().await;
    server.stop().await;
}

/// A call in a session the server signed out is told to sign in, at once:
/// not left waiting for a reconnect as if the network had failed. Here
/// that reconnect would never come: as a device with no session, the TV
/// is past the cap.
#[tokio::test]
async fn a_call_after_the_device_is_signed_out_asks_for_a_sign_in() {
    let (server, tv, alice, _stranger) = full_of_strangers("sdk-signed-out-call").await;
    let mut state = tv.state(&id(&server)).unwrap();
    server.store.disable_user("alice").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.current() == ConnectionState::Connected {
            state.changed().await.expect("the client is open");
        }
    })
    .await
    .expect("the connection was not closed");
    let started = std::time::Instant::now();
    assert_eq!(
        alice.libraries().await.unwrap_err(),
        SdkError::SignInRequired
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    // Signing in again is refused while the server has no room: the TV
    // is a device with no session now.
    assert_eq!(
        tv.sign_in(&id(&server), "alice", "correct horse", false)
            .await
            .unwrap_err(),
        SdkError::ServerFull
    );
    tv.close().await;
    server.stop().await;
}

/// Past the cap on devices with no session, a new one is refused with a
/// reason the app can show: the server is full, try again later. Not that
/// it cannot be reached.
#[tokio::test]
async fn a_new_device_past_the_stranger_cap_is_told_the_server_is_full() {
    let (server, tv, _alice, _stranger) = full_of_strangers("sdk-server-full").await;
    let newcomer = Client::open(
        Arc::new(MemoryStorage::default()),
        device(mkv_capabilities()),
    )
    .await
    .unwrap();
    let addrs = server.server.addr().ip_addrs().copied().collect();
    newcomer.add_address(&id(&server), addrs).unwrap();
    assert_eq!(
        newcomer.connect(&id(&server)).await.unwrap_err(),
        SdkError::ServerFull
    );
    newcomer.close().await;
    tv.close().await;
    server.stop().await;
}

/// Every session on the server as if it expired: what a long absence does.
async fn expire_sessions(server: &TestServer) {
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(server.dir.join("spoolframe.db")),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE sessions SET expires_at = expires_at - 100000000")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// P4-01 through the SDK: remembered at sign-in, listed, resumed after the
/// session ended, and forgotten by signing out.
#[tokio::test]
async fn a_remembered_profile_resumes_without_a_password_until_signed_out() {
    let (server, client, _) = ready("sdk-trust").await;
    let alice = client
        .sign_in(&id(&server), "alice", "correct horse", true)
        .await
        .unwrap();
    assert_eq!(alice.remembered(), Some(true));
    let bob = client
        .sign_in(&id(&server), "bob", "battery staple", false)
        .await
        .unwrap();
    assert_eq!(bob.remembered(), Some(false));
    let profiles = client.trusted_profiles(&id(&server)).await.unwrap();
    assert_eq!(
        profiles
            .iter()
            .map(|p| p.username.as_str())
            .collect::<Vec<_>>(),
        ["alice"]
    );
    assert_eq!(profiles[0].user_id, alice.user().user_id);

    expire_sessions(&server).await;
    // Found ended, the profile stays for "Who's watching".
    assert!(client.profiles(&id(&server)).await.unwrap().is_empty());
    let resumed = client
        .resume_profile(&id(&server), &profiles[0].user_id)
        .await
        .unwrap();
    assert_eq!(resumed.user().username, "alice");
    resumed.libraries().await.unwrap();
    // A resume is no sign-in: how she last signed in is still the password.
    assert_eq!(
        methods(&client, &server).await,
        [
            ("alice".to_owned(), Some(SignInMethod::Password)),
            ("bob".to_owned(), Some(SignInMethod::Password)),
        ]
    );
    assert_eq!(
        client
            .resume_profile(&id(&server), &bob.user().user_id)
            .await
            .unwrap_err(),
        SdkError::SignInRequired
    );

    resumed.sign_out().await.unwrap();
    assert!(
        client
            .trusted_profiles(&id(&server))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        client
            .resume_profile(&id(&server), &profiles[0].user_id)
            .await
            .unwrap_err(),
        SdkError::SignInRequired
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_session_sets_its_own_device_s_trust_and_auto_login() {
    let (server, client, _) = ready("sdk-set-trust").await;
    let alice = alice(&server, &client).await;
    alice.set_device_trust(None, true, true).await.unwrap();
    let devices = alice.devices().await.unwrap();
    assert!(devices[0].trusted && devices[0].auto_login, "{devices:?}");
    let profile = &client.trusted_profiles(&id(&server)).await.unwrap()[0];
    assert!(profile.auto_login);
    // Stopping ends the session here; the profile stays to sign in again.
    alice.set_device_trust(None, false, false).await.unwrap();
    let saved = client.saved_profiles(&id(&server)).await.unwrap();
    assert_eq!(saved[0].sign_in, ProfileSignIn::Password);
    assert!(
        client
            .trusted_profiles(&id(&server))
            .await
            .unwrap()
            .is_empty()
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_tv_s_request_to_be_remembered_is_the_approver_s_to_grant() {
    let (server, phone, _) = ready("sdk-trust-approval").await;
    let alice = alice(&server, &phone).await;
    let tv = client_on(&server, &Arc::new(MemoryStorage::default())).await;
    for approve in [false, true] {
        let sign_in = tv.begin_device_sign_in(&id(&server), true).await.unwrap();
        assert!(sign_in.remember);
        let waiting = {
            let tv = tv.clone();
            let sign_in = sign_in.clone();
            tokio::spawn(async move { tv.await_device_sign_in(&sign_in).await })
        };
        assert!(
            alice
                .lookup_device_sign_in(&sign_in.code)
                .await
                .unwrap()
                .remember
        );
        alice
            .approve_device_sign_in(&sign_in.code, approve)
            .await
            .unwrap();
        let on_tv = waiting.await.unwrap().unwrap();
        assert_eq!(on_tv.remembered(), Some(approve));
    }
    tv.close().await;
    phone.close().await;
    server.stop().await;
}
