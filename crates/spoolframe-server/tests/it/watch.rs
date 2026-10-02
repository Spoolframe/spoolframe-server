//! Watch state per user, and Home (P1-14), over a real Iroh connection.

use crate::support;

use std::time::Duration;

use spoolframe_protocol::{HomeScreen, Reply, Request, Response, SessionId};
use spoolframe_types::{Item, MediaId, PlaybackState};
use support::{TestClient, TestServer, episode, movie};

async fn alice_and_bob(test: &str) -> (TestServer, TestClient, SessionId, SessionId) {
    let server = TestServer::start(test).await;
    server.add_user("alice", "correct horse", None).await;
    server.add_user("bob", "battery staple", None).await;
    let tv = server.client().await;
    tv.hello().await;
    let alice = tv.login("alice", "correct horse").await;
    let bob = tv.login("bob", "battery staple").await;
    (server, tv, alice, bob)
}

async fn progress(tv: &TestClient, who: &SessionId, id: &MediaId, secs: u64) {
    let response = tv
        .call(
            Some(who),
            Request::UpdatePlaybackProgress {
                id: id.clone(),
                position_ms: secs * 1000,
            },
        )
        .await;
    assert_eq!(response, Response::Ok(Reply::Done));
}

async fn mark_watched(tv: &TestClient, who: &SessionId, id: &MediaId) {
    let response = tv
        .call(Some(who), Request::MarkWatched { id: id.clone() })
        .await;
    assert_eq!(response, Response::Ok(Reply::Done));
}

async fn state_of(tv: &TestClient, who: &SessionId, id: &MediaId) -> PlaybackState {
    match tv.call(Some(who), Request::Item { id: id.clone() }).await {
        Response::Ok(Reply::Item {
            item: Item::Movie(m),
            ..
        }) => m.playback,
        Response::Ok(Reply::Item {
            item: Item::Episode(e),
            ..
        }) => e.playback,
        other => panic!("no item: {other:?}"),
    }
}

async fn home(tv: &TestClient, who: &SessionId) -> HomeScreen {
    match tv.call(Some(who), Request::Home).await {
        Response::Ok(Reply::Home(home)) => home,
        other => panic!("no home: {other:?}"),
    }
}

fn ids(items: &[Item]) -> Vec<String> {
    items.iter().map(|i| i.id().to_string()).collect()
}

#[tokio::test]
async fn each_user_has_their_own_progress() {
    let (server, tv, alice, bob) = alice_and_bob("two-users").await;
    let id = movie().id().clone();
    progress(&tv, &alice, &id, 600).await;
    assert_eq!(
        state_of(&tv, &alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
    assert_eq!(state_of(&tv, &bob, &id).await, PlaybackState::default());
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn progress_past_the_threshold_marks_watched() {
    let (server, tv, alice, _) = alice_and_bob("threshold").await;
    let id = movie().id().clone();
    // The fake movie is 7020 s long: 90% is 6318 s.
    progress(&tv, &alice, &id, 6400).await;
    let state = state_of(&tv, &alice, &id).await;
    assert!(state.watched);
    assert_eq!(state.position, None);
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn under_a_minute_is_not_in_progress() {
    let (server, tv, alice, _) = alice_and_bob("not-started").await;
    let id = movie().id().clone();
    progress(&tv, &alice, &id, 30).await;
    assert_eq!(state_of(&tv, &alice, &id).await.position, None);
    assert!(home(&tv, &alice).await.continue_watching.is_empty());
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn fast_updates_are_coalesced() {
    let (server, tv, alice, _) = alice_and_bob("throttle").await;
    let id = movie().id().clone();
    progress(&tv, &alice, &id, 600).await;
    // Within the throttle: not written.
    progress(&tv, &alice, &id, 610).await;
    progress(&tv, &alice, &id, 620).await;
    assert_eq!(
        state_of(&tv, &alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
    // Crossing into watched is always written.
    progress(&tv, &alice, &id, 6900).await;
    assert!(state_of(&tv, &alice, &id).await.watched);
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn continue_watching_is_newest_first() {
    let (server, tv, alice, _) = alice_and_bob("continue").await;
    let movie_id = movie().id().clone();
    let episode_id = episode("13", "11", 1, 2).id().clone();
    progress(&tv, &alice, &movie_id, 600).await;
    // updated_at has one-second resolution.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    progress(&tv, &alice, &episode_id, 300).await;
    let home = home(&tv, &alice).await;
    assert_eq!(
        ids(&home.continue_watching),
        [episode_id.to_string(), movie_id.to_string()]
    );
    let Item::Episode(e) = &home.continue_watching[0] else {
        panic!("not an episode");
    };
    assert_eq!(e.playback.position, Some(Duration::from_secs(300)));
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn next_up_crosses_a_season_boundary() {
    let (server, tv, alice, _) = alice_and_bob("next-up").await;
    mark_watched(&tv, &alice, episode("12", "11", 1, 1).id()).await;
    mark_watched(&tv, &alice, episode("13", "11", 1, 2).id()).await;
    let home = home(&tv, &alice).await;
    // Season 2, episode 1; specials (season 0) are skipped.
    assert_eq!(ids(&home.next_up), ["plex:episode:15"]);
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn next_up_is_after_the_latest_watched_episode() {
    let (server, tv, alice, _) = alice_and_bob("next-up-gap").await;
    // Watched E2 but not E1: next up is what follows E2.
    mark_watched(&tv, &alice, episode("13", "11", 1, 2).id()).await;
    assert_eq!(ids(&home(&tv, &alice).await.next_up), ["plex:episode:15"]);
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn pages_and_searches_carry_the_user_s_state() {
    let (server, tv, alice, bob) = alice_and_bob("filled").await;
    let id = movie().id().clone();
    progress(&tv, &alice, &id, 600).await;
    let library = support::library().id;
    let Response::Ok(Reply::Items { items, .. }) = tv
        .call(
            Some(&alice),
            Request::Items {
                library: library.clone(),
                offset: 0,
                limit: 10,
            },
        )
        .await
    else {
        panic!("no page");
    };
    let Item::Movie(m) = &items[0] else {
        panic!("not a movie")
    };
    assert_eq!(m.playback.position, Some(Duration::from_secs(600)));

    let Response::Ok(Reply::Search { items }) = tv
        .call(
            Some(&bob),
            Request::Search {
                query: "alien".into(),
                limit: 10,
            },
        )
        .await
    else {
        panic!("no search");
    };
    let Item::Movie(m) = &items[0] else {
        panic!("not a movie")
    };
    assert_eq!(m.playback, PlaybackState::default(), "bob has no progress");
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn home_lists_recently_added() {
    let (server, tv, alice, _) = alice_and_bob("recent").await;
    assert_eq!(
        ids(&home(&tv, &alice).await.recently_added),
        [movie().id().to_string()]
    );
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn shows_have_no_watch_state() {
    let (server, tv, alice, _) = alice_and_bob("not-tracked").await;
    let response = tv
        .call(
            Some(&alice),
            Request::MarkWatched {
                id: support::show().id().clone(),
            },
        )
        .await;
    assert!(matches!(
        response,
        Response::Err(spoolframe_protocol::Error::NotFound { .. })
    ));
    tv.close().await;
    server.stop().await;
}
