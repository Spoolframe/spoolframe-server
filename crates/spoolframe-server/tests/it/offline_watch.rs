//! Offline watch sync (PX-38, `spoolframe:downloads`): where a device got
//! to while it had no server, sent when it reaches it again. Newest wins,
//! by the server's clock once the device's is set right.

use std::time::Duration;

use spoolframe_protocol::downloads::{OfflineWatch, OfflineWatchOutcome};
use spoolframe_protocol::features::DOWNLOADS;
use spoolframe_protocol::{Error, Hello, PROTOCOL_VERSION, Reply, Request, Response, SessionId};
use spoolframe_types::{MediaId, MediaKind, Namespace, Platform};

use crate::support::{TestClient, TestServer, movie};
use crate::watch::state_of;

/// A server with alice, and a Linux app signed in as her that lists
/// downloads, or not.
async fn signed_in(test: &str, listed: bool) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start(test).await;
    server.add_user("alice", "correct horse", None).await;
    let laptop = server.client().await;
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "test".into(),
        client_version: "0".into(),
        platform: Platform::Linux,
        device_name: None,
        capabilities: None,
        experimental: Vec::new(),
        features: if listed {
            vec![DOWNLOADS.to_owned()]
        } else {
            Vec::new()
        },
    };
    assert!(matches!(
        laptop.call(None, Request::Hello(hello)).await,
        Response::Ok(Reply::ServerInfo(_))
    ));
    let session = laptop.login("alice", "correct horse").await;
    (server, laptop, session)
}

/// Episode 15: 44 minutes, its credits from 40:00 to its end.
fn with_credits() -> MediaId {
    MediaId::new(Namespace::Plex, MediaKind::Episode, "15").unwrap()
}

/// At `position` seconds into `id`, `ago` seconds before `device_now`.
fn at(id: &MediaId, position: u64, device_now: i64, ago: i64) -> OfflineWatch {
    OfflineWatch {
        id: id.clone(),
        position_ms: Some(position * 1000),
        watched: None,
        at: device_now - ago,
    }
}

async fn sync(
    laptop: &TestClient,
    session: &SessionId,
    device_now: i64,
    entries: Vec<OfflineWatch>,
) -> Response {
    laptop
        .call(
            Some(session),
            Request::SyncOfflineWatch {
                device_now,
                entries,
            },
        )
        .await
}

async fn synced(
    laptop: &TestClient,
    session: &SessionId,
    device_now: i64,
    entries: Vec<OfflineWatch>,
) -> Vec<OfflineWatchOutcome> {
    match sync(laptop, session, device_now, entries).await {
        Response::Ok(Reply::OfflineWatchSynced { outcomes }) => outcomes,
        other => panic!("not synced: {other:?}"),
    }
}

/// Two devices' offline states of one film: the later one stays, sent
/// first or last, and the throttle holds back neither.
#[tokio::test]
async fn the_newest_offline_state_wins_whichever_is_sent_last() {
    let (server, laptop, session) = signed_in("offline-newest", true).await;
    let id = movie().id().clone();
    let now = server.clock.unix_secs();
    let outcomes = synced(
        &laptop,
        &session,
        now,
        vec![at(&id, 600, now, 300), at(&id, 900, now, 100)],
    )
    .await;
    let then = synced(&laptop, &session, now, vec![at(&id, 300, now, 200)]).await;
    assert_eq!(
        (
            outcomes,
            then,
            state_of(&laptop, &session, &id).await.position
        ),
        (
            vec![OfflineWatchOutcome::Applied, OfflineWatchOutcome::Applied],
            vec![OfflineWatchOutcome::Older],
            Some(Duration::from_secs(900))
        )
    );
    server.stop().await;
}

/// The server's state, marked after the device played, stays.
#[tokio::test]
async fn an_offline_state_older_than_the_server_s_is_older() {
    let (server, laptop, session) = signed_in("offline-older", true).await;
    let id = movie().id().clone();
    let marked = laptop
        .call(Some(&session), Request::MarkWatched { id: id.clone() })
        .await;
    assert_eq!(marked, Response::Ok(Reply::Done));
    server.clock.advance(Duration::from_secs(600));
    let now = server.clock.unix_secs();
    let outcomes = synced(&laptop, &session, now, vec![at(&id, 60, now, 900)]).await;
    assert_eq!(
        (outcomes, state_of(&laptop, &session, &id).await.watched),
        (vec![OfflineWatchOutcome::Older], true)
    );
    server.stop().await;
}

/// A device whose clock is an hour ahead: what it played 15 minutes ago
/// is older than a mark made 10 minutes ago, and what it played 5 minutes
/// ago newer. Taken by its own clock, both would be "now".
#[tokio::test]
async fn a_device_clock_an_hour_ahead_is_set_right() {
    let (server, laptop, session) = signed_in("offline-skew", true).await;
    let id = movie().id().clone();
    let marked = laptop
        .call(Some(&session), Request::MarkWatched { id: id.clone() })
        .await;
    assert_eq!(marked, Response::Ok(Reply::Done));
    server.clock.advance(Duration::from_secs(600));
    let device_now = server.clock.unix_secs() + 3600;
    let outcomes = synced(
        &laptop,
        &session,
        device_now,
        vec![at(&id, 60, device_now, 900), at(&id, 120, device_now, 300)],
    )
    .await;
    assert_eq!(
        (outcomes, state_of(&laptop, &session, &id).await.position),
        (
            vec![OfflineWatchOutcome::Older, OfflineWatchOutcome::Applied],
            Some(Duration::from_secs(120))
        )
    );
    server.stop().await;
}

/// Past the credits' start (40:00) the episode is watched, before 95% of
/// its 44 minutes; just before them it is not.
#[tokio::test]
async fn watched_is_judged_from_the_position_and_the_credits() {
    let (server, laptop, session) = signed_in("offline-credits", true).await;
    let id = with_credits();
    let now = server.clock.unix_secs();
    synced(&laptop, &session, now, vec![at(&id, 2390, now, 200)]).await;
    let before = state_of(&laptop, &session, &id).await.watched;
    synced(&laptop, &session, now, vec![at(&id, 2450, now, 100)]).await;
    let after = state_of(&laptop, &session, &id).await.watched;
    assert_eq!((before, after), (false, true));
    server.stop().await;
}

/// An item the server does not have, and one with no watch state (a
/// show), are not tracked.
#[tokio::test]
async fn an_entry_for_an_item_without_watch_state_is_not_tracked() {
    let (server, laptop, session) = signed_in("offline-untracked", true).await;
    let now = server.clock.unix_secs();
    let gone = MediaId::new(Namespace::Plex, MediaKind::Movie, "999").unwrap();
    let show = MediaId::new(Namespace::Plex, MediaKind::Show, "10").unwrap();
    let outcomes = synced(
        &laptop,
        &session,
        now,
        vec![at(&gone, 60, now, 10), at(&show, 60, now, 10)],
    )
    .await;
    assert_eq!(outcomes, [OfflineWatchOutcome::NotTracked; 2]);
    server.stop().await;
}

/// Review 2026-10-20, finding 7: an entry for another catalog's item is
/// not tracked, and the rest of the batch is taken in.
#[tokio::test]
async fn an_entry_for_another_catalog_s_item_does_not_fail_the_batch() {
    let (server, laptop, session) = signed_in("offline-foreign", true).await;
    let now = server.clock.unix_secs();
    let foreign = MediaId::new(Namespace::Plex, MediaKind::Movie, "77").unwrap();
    server
        .catalog
        .set_old_key(&foreign, spoolframe_server::test_support::OldKey::Foreign);
    let id = movie().id().clone();
    let outcomes = synced(
        &laptop,
        &session,
        now,
        vec![at(&foreign, 60, now, 10), at(&id, 600, now, 10)],
    )
    .await;
    assert_eq!(
        outcomes,
        [
            OfflineWatchOutcome::NotTracked,
            OfflineWatchOutcome::Applied
        ]
    );
    server.stop().await;
}

#[tokio::test]
async fn an_entry_with_nothing_in_it_is_refused() {
    let (server, laptop, session) = signed_in("offline-empty", true).await;
    let now = server.clock.unix_secs();
    let empty = OfflineWatch {
        id: movie().id().clone(),
        position_ms: None,
        watched: None,
        at: now,
    };
    assert!(matches!(
        sync(&laptop, &session, now, vec![empty]).await,
        Response::Err(Error::InvalidInput { .. })
    ));
    server.stop().await;
}

#[tokio::test]
async fn a_client_that_did_not_list_downloads_syncs_nothing() {
    let (server, laptop, session) = signed_in("offline-unlisted", false).await;
    let id = movie().id().clone();
    let now = server.clock.unix_secs();
    let refused = sync(&laptop, &session, now, vec![at(&id, 600, now, 10)]).await;
    assert_eq!(
        (
            matches!(refused, Response::Err(Error::Unsupported { .. })),
            state_of(&laptop, &session, &id).await.position
        ),
        (true, None)
    );
    server.stop().await;
}
