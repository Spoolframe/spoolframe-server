//! Watch state per user, and Home (P1-14), over a real Iroh connection:
//! that the requests reach [`Watch`] and its state reaches what a client
//! is sent. The rules themselves are tested in `watch::tests`.

use crate::support;

use std::sync::Arc;
use std::time::Duration;

use spoolframe_plex::PlexCatalog;
use spoolframe_protocol::{HomeScreen, Reply, Request, Response, SessionId, TopResult};
use spoolframe_server::catalog::{Composite, LiveCatalog};
use spoolframe_types::{Item, MediaId, MediaKind, Namespace, PlaybackState};
use support::{ServerOptions, TestClient, TestServer, movie};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

/// One episode as a Plex listing or item has it: show 100, its seasons
/// 101 (specials), 102 and 103.
fn plex_episode(key: u32, season: u32, episode: u32) -> serde_json::Value {
    serde_json::json!({
        "ratingKey": key.to_string(),
        "type": "episode",
        "title": format!("S{season}E{episode}"),
        "grandparentRatingKey": "100",
        "grandparentTitle": "The Show",
        "parentRatingKey": (101 + season).to_string(),
        "parentIndex": season,
        "index": episode,
    })
}

fn plex_container(metadata: Vec<serde_json::Value>) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "MediaContainer": { "librarySectionID": 1, "Metadata": metadata }
    }))
}

/// A PMS with one show: specials (season 0: 201), season 1 (211, 212)
/// and season 2 (221, 222).
async fn pms_with_a_show() -> MockServer {
    let pms = MockServer::start().await;
    let episodes = [
        (201, 0, 1),
        (211, 1, 1),
        (212, 1, 2),
        (221, 2, 1),
        (222, 2, 2),
    ];
    let season = |s: u32| {
        serde_json::json!({
            "ratingKey": (101 + s).to_string(),
            "type": "season",
            "title": format!("Season {s}"),
            "parentRatingKey": "100",
            "parentTitle": "The Show",
            "index": s,
        })
    };
    let mount = |at: String, body: ResponseTemplate| {
        let pms = &pms;
        async move {
            Mock::given(method("GET"))
                .and(path(at))
                .respond_with(body)
                .mount(pms)
                .await;
        }
    };
    mount(
        "/library/metadata/100".into(),
        plex_container(vec![serde_json::json!({
            "ratingKey": "100",
            "type": "show",
            "title": "The Show",
            "leafCount": episodes.len(),
            "childCount": 3,
        })]),
    )
    .await;
    mount(
        "/library/metadata/100/children".into(),
        plex_container((0..3).map(season).collect()),
    )
    .await;
    mount(
        "/library/metadata/100/allLeaves".into(),
        plex_container(
            episodes
                .iter()
                .map(|&(key, s, e)| plex_episode(key, s, e))
                .collect(),
        ),
    )
    .await;
    for s in 0..3 {
        mount(
            format!("/library/metadata/{}", 101 + s),
            plex_container(vec![season(s)]),
        )
        .await;
        mount(
            format!("/library/metadata/{}/children", 101 + s),
            plex_container(
                episodes
                    .iter()
                    .filter(|&&(_, of, _)| of == s)
                    .map(|&(key, s, e)| plex_episode(key, s, e))
                    .collect(),
            ),
        )
        .await;
    }
    for &(key, s, e) in &episodes {
        mount(
            format!("/library/metadata/{key}"),
            plex_container(vec![plex_episode(key, s, e)]),
        )
        .await;
    }
    mount("/library/recentlyAdded".into(), plex_container(Vec::new())).await;
    pms
}

/// Next up walks a show's episodes in one request, not one per season
/// and two for the show: Home asks the PMS for the show's episodes and
/// for what was recently added, and nothing else.
#[tokio::test]
async fn home_asks_the_pms_for_a_show_s_episodes_at_once() {
    let pms = pms_with_a_show().await;
    let plex = PlexCatalog::new(pms.uri().parse().unwrap(), None).unwrap();
    let live = LiveCatalog::new(Some(Arc::new(Composite::single("fulcrum-ng", plex))));
    let options = ServerOptions {
        catalog: Some(Arc::new(live)),
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(support::fresh_dir("home-pms").await, options).await;
    server.add_user("alice", "correct horse", None).await;
    let tv = server.client().await;
    tv.hello().await;
    let alice = tv.login("alice", "correct horse").await;
    let last_of_season_one = MediaId::new(Namespace::Plex, MediaKind::Episode, "212").unwrap();
    let marked = tv
        .call(
            Some(&alice),
            Request::MarkWatched {
                id: last_of_season_one,
            },
        )
        .await;
    assert_eq!(marked, Response::Ok(Reply::Done));
    let before = pms.received_requests().await.unwrap().len();

    let home = home(&tv, &alice).await;

    let asked: Vec<String> = pms.received_requests().await.unwrap()[before..]
        .iter()
        .map(|r| r.url.path().to_owned())
        .collect();
    let next: Vec<String> = home.next_up.iter().map(|i| i.id().to_string()).collect();
    assert_eq!(
        next,
        [MediaId::new(Namespace::Plex, MediaKind::Episode, "221")
            .unwrap()
            .to_string()]
    );
    assert_eq!(asked.len(), 2, "{asked:?}");
    tv.close().await;
    server.stop().await;
}
