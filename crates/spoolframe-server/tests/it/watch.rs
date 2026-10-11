//! Watch state per user, and Home (P1-14), over a real Iroh connection:
//! that the requests reach [`Watch`] and its state reaches what a client
//! is sent. The rules themselves are tested in `watch::tests`.

use crate::support;

use std::time::Duration;

use spoolframe_protocol::{HomeScreen, Reply, Request, Response, SessionId, TopResult};
use spoolframe_types::{Item, MediaId, PlaybackState};
use support::{TestClient, TestServer, movie};

async fn alice_and_bob(test: &str) -> (TestServer, TestClient, SessionId, SessionId) {
    let server = TestServer::start(test).await;
    server.add_user("alice", "correct horse", None).await;
    server.add_user("bob", "battery staple", None).await;
    let tv = server.client().await;
    let mut capabilities = support::mkv_capabilities();
    capabilities
        .video
        .insert(spoolframe_types::VideoCodec::H264);
    tv.hello_with(None, Some(capabilities)).await;
    let alice = tv.login("alice", "correct horse").await;
    let bob = tv.login("bob", "battery staple").await;
    (server, tv, alice, bob)
}

async fn progress(server: &TestServer, tv: &TestClient, who: &SessionId, id: &MediaId, secs: u64) {
    server.write_media(1000);
    server.write_episode(id.key());
    let started = tv
        .call(
            Some(who),
            Request::StartPlayback {
                id: id.clone(),
                selection: Some(Default::default()),
                start_ms: None,
                bitmap: Default::default(),
                quality: Default::default(),
                display: None,
            },
        )
        .await;
    let Response::Ok(Reply::PlaybackStarted { handle, .. }) = started else {
        panic!("progress needs a playback: {started:?}")
    };
    let response = tv
        .call(
            Some(who),
            Request::UpdatePlaybackProgress {
                handle,
                sequence: 1,
                position_ms: secs * 1000,
            },
        )
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

#[tokio::test]
async fn each_user_has_their_own_progress() {
    let (server, tv, alice, bob) = alice_and_bob("two-users").await;
    let id = movie().id().clone();
    progress(&server, &tv, &alice, &id, 600).await;
    assert_eq!(
        state_of(&tv, &alice, &id).await.position,
        Some(Duration::from_secs(600))
    );
    assert_eq!(state_of(&tv, &bob, &id).await, PlaybackState::default());
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn pages_and_searches_carry_the_user_s_state() {
    let (server, tv, alice, bob) = alice_and_bob("filled").await;
    let id = movie().id().clone();
    progress(&server, &tv, &alice, &id, 600).await;
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

    let searched = |session| {
        let tv = &tv;
        async move {
            let Response::Ok(Reply::Search(results)) = tv
                .call(Some(session), Request::Search(support::search("alien")))
                .await
            else {
                panic!("no search");
            };
            let Some(TopResult::Item(Item::Movie(m))) = results.top else {
                panic!("not a movie")
            };
            m.playback
        }
    };
    assert_eq!(
        searched(&alice).await.position,
        Some(Duration::from_secs(600))
    );
    assert_eq!(
        searched(&bob).await,
        PlaybackState::default(),
        "bob has no progress"
    );
    tv.close().await;
    server.stop().await;
}

async fn hide(tv: &TestClient, who: &SessionId, id: &MediaId) {
    let response = tv
        .call(
            Some(who),
            Request::HideFromContinueWatching { id: id.clone() },
        )
        .await;
    assert_eq!(response, Response::Ok(Reply::Done));
}

#[tokio::test]
async fn a_hide_survives_a_restart() {
    let (server, tv, alice, _) = alice_and_bob("hide-restart").await;
    let id = movie().id().clone();
    progress(&server, &tv, &alice, &id, 600).await;
    hide(&tv, &alice, &id).await;
    tv.close().await;
    let server = server.restart().await;
    let tv = server.client().await;
    let mut capabilities = support::mkv_capabilities();
    capabilities
        .video
        .insert(spoolframe_types::VideoCodec::H264);
    tv.hello_with(None, Some(capabilities)).await;
    let alice = tv.login("alice", "correct horse").await;
    assert!(home(&tv, &alice).await.continue_watching.is_empty());
    tv.close().await;
    server.stop().await;
}
