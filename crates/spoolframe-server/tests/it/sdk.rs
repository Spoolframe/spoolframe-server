//! The client SDK (P1-15) against a real server in-process. The server is
//! off the LAN here; the SDK is given its address, as a typed-in address
//! would be. Discovery is tested in `lan.rs`.

use crate::support;

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Item, PlaybackState};
use spoolframe_client_sdk::{
    ArtworkSize, Client, ConnectionState, Device, MemoryStorage, SdkError, Selection, Session,
    Storage,
};
use support::{TestServer, episode, library, mkv_capabilities, movie, poster, show};

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
    let client = Client::open(storage.clone(), device(mkv_capabilities()))
        .await
        .unwrap();
    let addrs = server.server.addr().ip_addrs().copied().collect();
    client
        .add_address(&server.server.id().to_string(), addrs)
        .unwrap();
    client
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
