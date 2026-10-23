//! Play history (PX-52, `spoolframe:play-history`) over a real Iroh
//! connection: each play a row, tracks included, counted by the rule in
//! `spoolframe_policy::plays`, with time played measured on the server's
//! clock; play counts on an item's details, and Recently played.
//!
//! The fake catalog's episodes and tracks are probed as Brooklyn Nine-Nine
//! S07E01, 1309.472 s long: a track that long counts at 4 minutes, an
//! episode at 95% (1244 s), with no credits marker.

use crate::support;

use std::time::Duration;

use spoolframe_protocol::features::PLAY_HISTORY;
use spoolframe_protocol::playlists::Kinds;
use spoolframe_protocol::{
    Error, Hello, PROTOCOL_VERSION, PlaybackHandle, Reply, Request, Response, SessionId,
};
use spoolframe_server::principal::Principal;
use spoolframe_server::store::plays::{NewPlay, PlayRow, PlayedKind};
use spoolframe_types::{Item, MediaId, MediaKind, Namespace, PlaybackState};
use support::{TestClient, TestServer};

use crate::portable_sign_in::{identity, session, sign_in};

const PASSWORD: &str = "correct horse";

fn id(kind: MediaKind, key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, kind, key).unwrap()
}

fn track() -> MediaId {
    id(MediaKind::Track, "32")
}

fn episode() -> MediaId {
    id(MediaKind::Episode, "12")
}

/// A client that plays MKV with H.264, listing `features`.
async fn device(server: &TestServer, features: &[&str]) -> TestClient {
    let client = server.client().await;
    let mut capabilities = support::mkv_capabilities();
    capabilities
        .video
        .insert(spoolframe_types::VideoCodec::H264);
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "test".into(),
        client_version: "0".into(),
        platform: spoolframe_types::Platform::Linux,
        device_name: Some("Living room".into()),
        capabilities: Some(capabilities),
        experimental: Vec::new(),
        features: features.iter().map(|n| n.to_string()).collect(),
    };
    assert!(matches!(
        client.call(None, Request::Hello(hello)).await,
        Response::Ok(Reply::ServerInfo(_))
    ));
    client
}

/// A server with Alice and Bob, and a TV listing the feature.
async fn household(test: &str) -> (TestServer, TestClient, SessionId, SessionId) {
    let server = TestServer::start(test).await;
    server.add_user("alice", PASSWORD, None).await;
    server.add_user("bob", "battery staple", None).await;
    let tv = device(&server, &[PLAY_HISTORY]).await;
    let alice = tv.login("alice", PASSWORD).await;
    let bob = tv.login("bob", "battery staple").await;
    (server, tv, alice, bob)
}

async fn principal(server: &TestServer, name: &str) -> Principal {
    let user = server.store.user_by_name(name).await.unwrap().unwrap();
    Principal::LocalUser(user.id)
}

/// Start playing `item` from `from_secs`; its handle.
async fn start(
    server: &TestServer,
    tv: &TestClient,
    who: &SessionId,
    item: &MediaId,
    from_secs: u64,
) -> PlaybackHandle {
    server.write_media(1000);
    server.write_episode(item.key());
    let started = tv
        .call(
            Some(who),
            Request::StartPlayback {
                id: item.clone(),
                selection: Some(Default::default()),
                start_ms: Some(from_secs * 1000),
                bitmap: Default::default(),
                quality: Default::default(),
                display: None,
                capabilities: None,
                onward_share: None,
            },
        )
        .await;
    let Response::Ok(Reply::PlaybackStarted { handle, .. }) = started else {
        panic!("no playback: {started:?}")
    };
    handle
}

/// After `wait`, the player reports `secs` as report `sequence`.
async fn report(
    server: &TestServer,
    tv: &TestClient,
    who: &SessionId,
    handle: &PlaybackHandle,
    sequence: u64,
    wait: Duration,
    secs: u64,
) {
    server.clock.advance(wait);
    let response = tv
        .call(
            Some(who),
            Request::UpdatePlaybackProgress {
                handle: handle.clone(),
                sequence,
                position_ms: secs * 1000,
            },
        )
        .await;
    assert_eq!(response, Response::Ok(Reply::Done));
}

async fn stop(tv: &TestClient, who: &SessionId, handle: PlaybackHandle) {
    let response = tv.call(Some(who), Request::StopPlayback { handle }).await;
    assert_eq!(response, Response::Ok(Reply::Done));
}

/// How often a player reports while it plays (PROTOCOL.md).
const EVERY: u64 = 10;

/// Played on from `from_secs` to `to_secs`, reported every [`EVERY`]
/// seconds from report `sequence`, as a player does; the next sequence.
#[allow(clippy::too_many_arguments)]
async fn play_on(
    server: &TestServer,
    tv: &TestClient,
    who: &SessionId,
    handle: &PlaybackHandle,
    sequence: u64,
    from_secs: u64,
    to_secs: u64,
) -> u64 {
    let (mut sequence, mut at) = (sequence, from_secs);
    while at < to_secs {
        let step = (to_secs - at).min(EVERY);
        at += step;
        report(
            server,
            tv,
            who,
            handle,
            sequence,
            Duration::from_secs(step),
            at,
        )
        .await;
        sequence += 1;
    }
    sequence
}

/// Played from the start for `secs`, reported as a player does, and
/// stopped.
async fn play_for(
    server: &TestServer,
    tv: &TestClient,
    who: &SessionId,
    item: &MediaId,
    secs: u64,
) {
    let handle = start(server, tv, who, item, 0).await;
    play_on(server, tv, who, &handle, 1, 0, secs).await;
    stop(tv, who, handle).await;
}

/// From now, every write to a play fails, as a full disk's does: play
/// history alone, for the rest of the store goes on.
async fn break_plays(server: &TestServer) {
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(server.dir.join("spoolframe.db")),
    )
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER plays_broken BEFORE UPDATE ON plays \
         BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

/// `item`'s playback state on its details.
async fn state_of(tv: &TestClient, who: &SessionId, item: &MediaId) -> PlaybackState {
    match tv.call(Some(who), Request::Item { id: item.clone() }).await {
        Response::Ok(Reply::Item {
            item: Item::Movie(m),
            ..
        }) => m.playback,
        Response::Ok(Reply::Item {
            item: Item::Episode(e),
            ..
        }) => e.playback,
        Response::Ok(Reply::Item {
            item: Item::Track(t),
            ..
        }) => t.playback,
        other => panic!("no item: {other:?}"),
    }
}

async fn recently_played(tv: &TestClient, who: &SessionId, kinds: Option<Kinds>) -> Response {
    tv.call(
        Some(who),
        Request::RecentlyPlayed {
            kinds,
            offset: 0,
            limit: 50,
        },
    )
    .await
}

/// Recently played's items, by id, with their total.
async fn recent_ids(tv: &TestClient, who: &SessionId, kinds: Option<Kinds>) -> (Vec<String>, u32) {
    match recently_played(tv, who, kinds).await {
        Response::Ok(Reply::Items { items, total, .. }) => {
            (items.iter().map(|i| i.id().to_string()).collect(), total)
        }
        other => panic!("no Recently played: {other:?}"),
    }
}

async fn plays(server: &TestServer, who: Principal) -> Vec<PlayRow> {
    server.store.all_plays(who).await.unwrap()
}

/// `who`'s plays once there are `n` and `done` holds for them: a play is
/// written in the background, after the request that moved it on.
async fn plays_when(
    server: &TestServer,
    who: Principal,
    n: usize,
    done: impl Fn(&[PlayRow]) -> bool,
) -> Vec<PlayRow> {
    let mut rows = plays(server, who).await;
    for _ in 0..250 {
        if rows.len() == n && done(&rows) {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        rows = plays(server, who).await;
    }
    panic!("never {n} plays as expected: {rows:?}");
}

/// `who`'s `n` plays, once each is closed.
async fn closed_plays(server: &TestServer, who: Principal, n: usize) -> Vec<PlayRow> {
    plays_when(server, who, n, |rows| rows.iter().all(|r| r.closed)).await
}

/// A track played for 5 minutes counts, by the 4-minute rule, and one
/// stopped at 1 minute is kept as a row that does not; neither is a
/// resume point in Continue watching. Its progress is no longer refused.
#[tokio::test]
async fn a_track_played_long_enough_counts_and_one_stopped_early_does_not() {
    let (server, tv, alice, _) = household("plays-track").await;
    let who = principal(&server, "alice").await;

    play_for(&server, &tv, &alice, &track(), 300).await;
    play_for(&server, &tv, &alice, &track(), 60).await;

    let rows = closed_plays(&server, who, 2).await;
    let seen: Vec<(bool, i64, bool)> = rows
        .iter()
        .map(|r| (r.counted, r.played_ms, r.closed))
        .collect();
    assert_eq!(seen, [(true, 300_000, true), (false, 60_000, true)]);
    assert_eq!(rows[0].kind, "track");
    assert_eq!(rows[0].album_id.as_deref(), Some("plex:album:31"));
    assert_eq!(rows[0].artist_id.as_deref(), Some("plex:artist:30"));
    assert_eq!(rows[0].path, "iroh");
    assert_eq!(rows[0].device_name.as_deref(), Some("Living room"));
    assert!(rows[0].playback_id.is_some());
    assert_eq!(rows[0].duration_ms, Some(1_309_472));

    let state = state_of(&tv, &alice, &track()).await;
    assert_eq!(state.play_count, 1);
    assert!(state.last_played_at.is_some());
    // No resume point for a track: no watch state, nothing to continue.
    assert_eq!(state.position, None);
    assert!(server.store.all_watch(who).await.unwrap().is_empty());
    let home = crate::watch::home(&tv, &alice).await;
    assert!(home.continue_watching.is_empty(), "{home:?}");
    tv.close().await;
    server.stop().await;
}

/// An episode watched through twice is two counted plays, and one watch
/// state; a seek to its end and a stop is not a play.
#[tokio::test]
async fn a_rewatch_is_a_second_play_and_a_seek_to_the_end_is_none() {
    let (server, tv, alice, _) = household("plays-rewatch").await;
    let who = principal(&server, "alice").await;
    for _ in 0..2 {
        play_for(&server, &tv, &alice, &episode(), 1250).await;
    }
    closed_plays(&server, who, 2).await;
    let state = state_of(&tv, &alice, &episode()).await;
    assert_eq!(state.play_count, 2);
    assert!(state.watched);
    assert_eq!(server.store.all_watch(who).await.unwrap().len(), 1);

    // Ten seconds in, the player seeks to the end, and is stopped.
    let handle = start(&server, &tv, &alice, &episode(), 0).await;
    report(
        &server,
        &tv,
        &alice,
        &handle,
        1,
        Duration::from_secs(10),
        1300,
    )
    .await;
    stop(&tv, &alice, handle).await;
    let rows = closed_plays(&server, who, 3).await;
    assert!(!rows[2].counted, "{:?}", rows[2]);
    // Ten seconds, and the few milliseconds the test itself took.
    assert!(
        (10_000..11_000).contains(&rows[2].played_ms),
        "{:?}",
        rows[2]
    );
    assert_eq!(state_of(&tv, &alice, &episode()).await.play_count, 2);
    tv.close().await;
    server.stop().await;
}

/// A play that is never stopped is closed as its playback expires, as
/// far as it had got.
#[tokio::test]
async fn a_play_whose_playback_expires_is_closed() {
    let (server, tv, alice, _) = household("plays-expire").await;
    let who = principal(&server, "alice").await;
    let handle = start(&server, &tv, &alice, &track(), 0).await;
    play_on(&server, &tv, &alice, &handle, 1, 0, 300).await;
    server.clock.advance(support::PLAYBACK_IDLE);
    server.debug.playbacks.sweep();
    let mut rows = plays(&server, who).await;
    for _ in 0..100 {
        if rows[0].closed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        rows = plays(&server, who).await;
    }
    assert!(rows[0].closed && rows[0].counted, "{rows:?}");
    tv.close().await;
    server.stop().await;
}

/// Recently played is the user's own, newest first, filtered by kind;
/// another user's plays never show, nor move their counts.
#[tokio::test]
async fn plays_are_per_user_and_recently_played_is_one_s_own() {
    let (server, tv, alice, bob) = household("plays-users").await;
    play_for(&server, &tv, &alice, &episode(), 1250).await;
    server.clock.advance(Duration::from_secs(60));
    play_for(&server, &tv, &alice, &track(), 300).await;
    play_for(&server, &tv, &bob, &id(MediaKind::Track, "33"), 300).await;
    // Started and stopped: history, not a play, so not listed.
    play_for(&server, &tv, &alice, &id(MediaKind::Track, "35"), 30).await;
    closed_plays(&server, principal(&server, "alice").await, 3).await;
    closed_plays(&server, principal(&server, "bob").await, 1).await;

    assert_eq!(
        recent_ids(&tv, &alice, None).await,
        (vec![track().to_string(), episode().to_string()], 2)
    );
    let music = Kinds {
        tracks: true,
        ..Kinds::default()
    };
    assert_eq!(
        recent_ids(&tv, &alice, Some(music)).await,
        (vec![track().to_string()], 1)
    );
    assert_eq!(
        recent_ids(&tv, &bob, None).await,
        (vec!["plex:track:33".to_owned()], 1)
    );
    match recently_played(&tv, &alice, None).await {
        Response::Ok(Reply::Items { items, .. }) => {
            let Item::Track(t) = &items[0] else {
                panic!("not a track: {items:?}")
            };
            assert_eq!(t.playback.play_count, 1);
        }
        other => panic!("no Recently played: {other:?}"),
    }
    assert_eq!(state_of(&tv, &bob, &track()).await.play_count, 0);
    tv.close().await;
    server.stop().await;
}

/// A client that did not list the feature gets no play counts, and its
/// Recently played is refused.
#[tokio::test]
async fn a_client_without_the_feature_gets_none_of_it() {
    let (server, tv, alice, _) = household("plays-unlisted").await;
    play_for(&server, &tv, &alice, &track(), 300).await;
    closed_plays(&server, principal(&server, "alice").await, 1).await;
    assert_eq!(state_of(&tv, &alice, &track()).await.play_count, 1);

    let phone = device(&server, &[]).await;
    let on_phone = phone.login("alice", PASSWORD).await;
    let state = state_of(&phone, &on_phone, &track()).await;
    assert_eq!((state.play_count, state.last_played_at), (0, None));
    assert!(matches!(
        recently_played(&phone, &on_phone, None).await,
        Response::Err(Error::Unsupported { .. })
    ));
    phone.close().await;
    tv.close().await;
    server.stop().await;
}

/// A portable user who leaves the server takes their plays with them.
#[tokio::test]
async fn a_user_who_leaves_takes_their_plays() {
    let server = TestServer::start("plays-leave").await;
    let sam = identity(7);
    server
        .store
        .grant_portable(sam, Some("Sam"), None)
        .await
        .unwrap();
    let tv = server.client().await;
    tv.hello_as(Some("Living room")).await;
    let token = session(sign_in(&tv, 7, 9).await);
    let who = Principal::PortableUser(sam);
    let external_ids = ["test://32".to_owned()];
    server
        .store
        .open_play(
            who,
            NewPlay {
                media_id: "plex:track:32",
                kind: PlayedKind::Track,
                show_id: None,
                album_id: None,
                artist_id: None,
                external_ids: &external_ids,
                duration_ms: None,
                endpoint_id: None,
                device_name: None,
                playback_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(plays(&server, who).await.len(), 1);

    assert_eq!(
        tv.call(Some(&token), Request::LeaveServer).await,
        Response::Ok(Reply::Done)
    );
    assert!(plays(&server, who).await.is_empty());
    // Their external ids went too: nothing re-links to them.
    let under = server
        .store
        .plays_under_another_key(who, "plex:track:99", &external_ids)
        .await
        .unwrap();
    assert_eq!(under, None);
    server.stop().await;
}

/// A player that went quiet for ten minutes and then reports a position
/// ten minutes on was paused, backgrounded or cut off, and seeked: the
/// gap credits what one report can, not the ten minutes.
#[tokio::test]
async fn a_long_silence_before_a_report_credits_no_more_than_one_report_can() {
    let (server, tv, alice, _) = household("plays-silence").await;
    let who = principal(&server, "alice").await;
    let handle = start(&server, &tv, &alice, &track(), 0).await;
    report(
        &server,
        &tv,
        &alice,
        &handle,
        1,
        Duration::from_secs(10),
        10,
    )
    .await;
    report(
        &server,
        &tv,
        &alice,
        &handle,
        2,
        Duration::from_secs(600),
        610,
    )
    .await;
    stop(&tv, &alice, handle).await;
    let rows = closed_plays(&server, who, 1).await;
    assert_eq!(
        (rows[0].played_ms, rows[0].counted),
        (40_000, false),
        "{rows:?}"
    );
    tv.close().await;
    server.stop().await;
}

/// A play that cannot be written fails nothing: the report is taken, its
/// sequence with it.
#[tokio::test]
async fn a_report_is_taken_when_its_play_cannot_be_written() {
    let (server, tv, alice, _) = household("plays-broken-report").await;
    let handle = start(&server, &tv, &alice, &track(), 0).await;
    break_plays(&server).await;
    // Past the throttle: the play is written, and fails.
    report(
        &server,
        &tv,
        &alice,
        &handle,
        1,
        Duration::from_secs(10),
        10,
    )
    .await;
    let again = tv
        .call(
            Some(&alice),
            Request::UpdatePlaybackProgress {
                handle: handle.clone(),
                sequence: 1,
                position_ms: 10_000,
            },
        )
        .await;
    assert_eq!(again, Response::Err(Error::StaleProgress));
    tv.close().await;
    server.stop().await;
}

/// With Plex down, a report's watch state fails, but its play still
/// moves on: an outage does not stop plays counting.
#[tokio::test]
async fn a_play_moves_on_while_the_catalog_is_down() {
    let (server, tv, alice, _) = household("plays-catalog-down").await;
    let who = principal(&server, "alice").await;
    let handle = start(&server, &tv, &alice, &episode(), 0).await;
    server.catalog.remove(&episode());
    server.clock.advance(Duration::from_secs(10));
    let response = tv
        .call(
            Some(&alice),
            Request::UpdatePlaybackProgress {
                handle: handle.clone(),
                sequence: 1,
                position_ms: 10_000,
            },
        )
        .await;
    assert!(matches!(response, Response::Err(_)), "{response:?}");
    plays_when(&server, who, 1, |rows| rows[0].played_ms == 10_000).await;
    tv.close().await;
    server.stop().await;
}

/// A stop whose play cannot be closed still stops the playback; the play
/// is left open, for the next start of the server to close.
#[tokio::test]
async fn a_stop_ends_the_playback_when_its_play_cannot_be_closed() {
    let (server, tv, alice, _) = household("plays-broken-stop").await;
    let who = principal(&server, "alice").await;
    let handle = start(&server, &tv, &alice, &track(), 0).await;
    // Opened: only its writes fail from here.
    plays_when(&server, who, 1, |_| true).await;
    break_plays(&server).await;
    stop(&tv, &alice, handle.clone()).await;
    let after = tv
        .call(
            Some(&alice),
            Request::UpdatePlaybackProgress {
                handle,
                sequence: 1,
                position_ms: 10_000,
            },
        )
        .await;
    assert!(
        matches!(
            after,
            Response::Err(Error::PlaybackStopped | Error::PlaybackExpired)
        ),
        "{after:?}"
    );
    assert!(!plays(&server, who).await[0].closed);
    tv.close().await;
    server.stop().await;
}

/// The item's plays under an earlier key come over as it starts playing,
/// before its new play takes its external ids.
#[tokio::test]
async fn a_start_brings_plays_under_an_earlier_key_over() {
    let (server, tv, alice, _) = household("plays-start-relink").await;
    let who = principal(&server, "alice").await;
    let external_ids = ["test://32".to_owned()];
    let old = server
        .store
        .open_play(
            who,
            NewPlay {
                media_id: "plex:track:999",
                kind: PlayedKind::Track,
                show_id: None,
                album_id: None,
                artist_id: None,
                external_ids: &external_ids,
                duration_ms: None,
                endpoint_id: None,
                device_name: None,
                playback_id: None,
            },
        )
        .await
        .unwrap();
    server.store.close_open_plays().await.unwrap();
    let handle = start(&server, &tv, &alice, &track(), 0).await;
    stop(&tv, &alice, handle).await;
    let rows = closed_plays(&server, who, 2).await;
    assert_eq!(rows[0].id, old);
    assert!(
        rows.iter().all(|r| r.media_id == track().to_string()),
        "{rows:?}"
    );
    tv.close().await;
    server.stop().await;
}
