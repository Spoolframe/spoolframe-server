//! `WatchChanged` (feature `spoolframe:watch-events`): a change to a
//! user's watch state is told to their other connections that named the
//! feature, and to no other.

use crate::support;

use std::time::Duration;

use iroh::endpoint::RecvStream;
use spoolframe_protocol::codec::read_frame;
use spoolframe_protocol::downloads::OfflineWatch;
use spoolframe_protocol::streams::uni;
use spoolframe_protocol::{
    Event, Feature, Features, PlaybackHandle, Reply, Request, Response, SessionId,
};
use spoolframe_types::{MediaId, MediaKind, Namespace};
use support::{TestClient, TestServer};
use tokio::io::AsyncReadExt;

const PASSWORD: &str = "correct horse";
/// How long a connection that is to be told nothing is watched.
const A_WHILE: Duration = Duration::from_millis(1500);

struct Device {
    client: TestClient,
    session: SessionId,
    events: RecvStream,
}

/// A device signed in as `username`, its `Hello` naming `named`.
async fn device(server: &TestServer, username: &str, named: Features) -> Device {
    let client = server.client().await;
    client.hello_with_features(named).await;
    let mut events = tokio::time::timeout(Duration::from_secs(5), client.conn.accept_uni())
        .await
        .expect("no events stream within 5 s")
        .unwrap();
    assert_eq!(events.read_u8().await.unwrap(), uni::EVENTS);
    let session = client.login(username, PASSWORD).await;
    // Noted as acting as the user from its first request.
    let who = client.call(Some(&session), Request::WhoAmI).await;
    assert!(matches!(who, Response::Ok(Reply::WhoAmI(_))), "{who:?}");
    Device {
        client,
        session,
        events,
    }
}

fn watching() -> Features {
    Features::only(Feature::WatchEvents)
}

/// The next `WatchChanged` on `events` within `wait`, past the others.
async fn watch_event(events: &mut RecvStream, wait: Duration) -> Option<Event> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let read = tokio::time::timeout_at(deadline, read_frame::<_, Event>(events)).await;
        match read {
            Err(_) | Ok(Err(_)) => return None,
            Ok(Ok(event @ Event::WatchChanged { .. })) => return Some(event),
            Ok(Ok(_)) => {}
        }
    }
}

async fn done(device: &Device, request: Request) {
    let answered = device.client.call(Some(&device.session), request).await;
    assert_eq!(answered, Response::Ok(Reply::Done));
}

/// A mark, and a hide, on the phone: the user's TV that named the feature
/// is told which item, the phone itself and a laptop that did not name
/// it are not, nor is another user's device.
#[tokio::test]
async fn a_change_is_told_to_the_user_s_other_devices_that_named_it() {
    let server = TestServer::start("watch-events").await;
    server.write_media(1000);
    for name in ["alice", "bob"] {
        server.add_user(name, PASSWORD, None).await;
    }
    let mut phone = device(&server, "alice", watching()).await;
    let mut tv = device(&server, "alice", watching()).await;
    let mut laptop = device(&server, "alice", Features::empty()).await;
    let mut bobs = device(&server, "bob", watching()).await;
    let film = support::movie().id().clone();

    done(&phone, Request::MarkWatched { id: film.clone() }).await;
    assert_eq!(
        watch_event(&mut tv.events, Duration::from_secs(5)).await,
        Some(Event::WatchChanged {
            items: vec![film.clone()]
        })
    );
    done(
        &phone,
        Request::HideFromContinueWatching { id: film.clone() },
    )
    .await;
    assert_eq!(
        watch_event(&mut tv.events, Duration::from_secs(5)).await,
        Some(Event::WatchChanged {
            items: vec![film.clone()]
        })
    );
    for (who, events) in [
        ("the phone that made it", &mut phone.events),
        ("a device without the feature", &mut laptop.events),
        ("another user's device", &mut bobs.events),
    ] {
        assert_eq!(watch_event(events, A_WHILE).await, None, "{who}");
    }
    server.stop().await;
}

/// Episode 12, probed as 1309 s long: watched from 1244 s, with no
/// credits marker.
fn episode() -> MediaId {
    MediaId::new(Namespace::Plex, MediaKind::Episode, "12").unwrap()
}

/// `device` plays `id` from the start.
async fn start(server: &TestServer, device: &Device, id: &MediaId) -> PlaybackHandle {
    server.write_media(1000);
    server.write_episode(id.key());
    let mut capabilities = support::mkv_capabilities();
    capabilities
        .video
        .insert(spoolframe_types::VideoCodec::H264);
    let started = device
        .client
        .call(
            Some(&device.session),
            Request::StartPlayback {
                id: id.clone(),
                selection: Some(Default::default()),
                start_ms: None,
                bitmap: Default::default(),
                quality: Default::default(),
                display: None,
                capabilities: Some(capabilities),
                onward_share: None,
            },
        )
        .await;
    let Response::Ok(Reply::PlaybackStarted { handle, .. }) = started else {
        panic!("no playback: {started:?}")
    };
    handle
}

/// Ten seconds on, `device`'s player reports `secs` as report `sequence`.
async fn report(
    server: &TestServer,
    device: &Device,
    handle: &PlaybackHandle,
    sequence: u64,
    secs: u64,
) {
    server.clock.advance(Duration::from_secs(10));
    done(
        device,
        Request::UpdatePlaybackProgress {
            handle: handle.clone(),
            sequence,
            position_ms: secs * 1000,
        },
    )
    .await;
}

/// A playback on the phone, reported every 10 s: the TV is told when the
/// episode comes into progress (at a minute), when it is watched, and
/// when it stops; not of the reports between, which only move it on.
#[tokio::test]
async fn a_playback_is_told_when_its_state_turns_not_on_every_report() {
    let server = TestServer::start("watch-events-progress").await;
    server.add_user("alice", PASSWORD, None).await;
    let phone = device(&server, "alice", watching()).await;
    let mut tv = device(&server, "alice", watching()).await;
    let episode = episode();
    let told = Some(Event::WatchChanged {
        items: vec![episode.clone()],
    });
    let handle = start(&server, &phone, &episode).await;

    for (sequence, secs) in (1..=5).zip((10..=50).step_by(10)) {
        report(&server, &phone, &handle, sequence, secs).await;
    }
    assert_eq!(
        watch_event(&mut tv.events, A_WHILE).await,
        None,
        "under a minute, it is not in progress yet"
    );
    report(&server, &phone, &handle, 6, 60).await;
    assert_eq!(
        watch_event(&mut tv.events, Duration::from_secs(5)).await,
        told,
        "in progress from a minute"
    );
    for (sequence, secs) in (7..=9).zip((70..=90).step_by(10)) {
        report(&server, &phone, &handle, sequence, secs).await;
    }
    assert_eq!(
        watch_event(&mut tv.events, A_WHILE).await,
        None,
        "moving on within progress"
    );
    report(&server, &phone, &handle, 10, 1250).await;
    assert_eq!(
        watch_event(&mut tv.events, Duration::from_secs(5)).await,
        told,
        "watched"
    );
    let stopped = phone
        .client
        .call(Some(&phone.session), Request::StopPlayback { handle })
        .await;
    assert_eq!(stopped, Response::Ok(Reply::Done));
    assert_eq!(
        watch_event(&mut tv.events, Duration::from_secs(5)).await,
        told,
        "stopped"
    );
    server.stop().await;
}

/// What a laptop played offline, synced: the TV is told the items written,
/// and not one the server had newer.
#[tokio::test]
async fn an_offline_sync_is_told_for_the_items_it_wrote() {
    let server = TestServer::start("watch-events-offline").await;
    server.write_media(1000);
    server.add_user("alice", PASSWORD, None).await;
    let laptop = device(&server, "alice", watching()).await;
    let mut tv = device(&server, "alice", watching()).await;
    let film = support::movie().id().clone();
    let now = server.clock.unix_secs();
    let at = |position: u64, ago: i64| OfflineWatch {
        id: film.clone(),
        position_ms: Some(position * 1000),
        watched: None,
        at: now - ago,
    };
    let sync = |entries: Vec<OfflineWatch>| Request::SyncOfflineWatch {
        device_now: now,
        entries,
    };

    let synced = laptop
        .client
        .call(Some(&laptop.session), sync(vec![at(600, 60)]))
        .await;
    assert!(
        matches!(synced, Response::Ok(Reply::OfflineWatchSynced { .. })),
        "{synced:?}"
    );
    assert_eq!(
        watch_event(&mut tv.events, Duration::from_secs(5)).await,
        Some(Event::WatchChanged {
            items: vec![film.clone()]
        })
    );
    // Older than what the server has now: nothing written, nothing told.
    let synced = laptop
        .client
        .call(Some(&laptop.session), sync(vec![at(300, 600)]))
        .await;
    assert!(
        matches!(synced, Response::Ok(Reply::OfflineWatchSynced { .. })),
        "{synced:?}"
    );
    assert_eq!(watch_event(&mut tv.events, A_WHILE).await, None);
    server.stop().await;
}

/// Marked through MCP (`set_watched`): the user's devices are told, as
/// none of them made it.
#[tokio::test]
async fn a_mark_through_mcp_is_told_to_the_user_s_devices() {
    let (server, addr) = crate::mcp::beside("watch-events-mcp").await;
    crate::mcp::user(&server, "alice", spoolframe_types::Role::Consumer).await;
    let mut tv = device(&server, "alice", watching()).await;
    let film = support::movie().id().clone();
    let mcp = crate::mcp::Mcp::connect(addr, "alice", &["library:read", "playlists:write"]).await;

    mcp.call(
        "set_watched",
        serde_json::json!({ "id": film.to_string(), "watched": true }),
    )
    .await
    .unwrap();

    assert_eq!(
        watch_event(&mut tv.events, Duration::from_secs(5)).await,
        Some(Event::WatchChanged { items: vec![film] })
    );
    server.stop().await;
}

/// The SDK names the feature by default and passes the events on.
#[tokio::test]
async fn the_sdk_passes_a_change_on() {
    let server = TestServer::start("watch-events-sdk").await;
    server.write_media(1000);
    server.add_user("alice", PASSWORD, None).await;
    let storage = std::sync::Arc::new(spoolframe_client_sdk::MemoryStorage::default());
    let client = crate::sdk::client_on(&server, &storage).await;
    let alice = client
        .sign_in(&server.server.id().to_string(), "alice", PASSWORD, false)
        .await
        .unwrap();
    assert!(alice.has_feature(Feature::WatchEvents));
    let mut events = client.watch_events();
    // Acting as alice on its connection from its first request.
    alice.whoami().await.unwrap();
    let phone = device(&server, "alice", watching()).await;
    let film = support::movie().id().clone();

    done(&phone, Request::MarkUnwatched { id: film.clone() }).await;

    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("no watch event within 5 s")
        .unwrap();
    assert_eq!(event.items, [film]);
    assert_eq!(event.server, server.server.id().to_string());
    client.close().await;
    server.stop().await;
}
