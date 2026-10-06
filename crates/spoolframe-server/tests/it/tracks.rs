//! Track preferences and the server's choice of tracks (P1-20) over a real
//! Iroh connection: that preferences reach the plan, and that a sent
//! selection is used as it is. The choosing itself is tested in
//! `tracks::tests`. Episodes 12 and 13 are probed as Brooklyn Nine-Nine
//! S07E01: English audio description before the main English 5.1, and an
//! "SDH" subtitle as the only English one.

use spoolframe_protocol::{
    ChosenTracks, Reply, Request, Response, Selection, SessionId, TrackReason,
};
use spoolframe_types::{
    MediaId, MediaKind, Namespace, SdhPreference, SubtitleMode, SubtitleRole, TrackPreferences,
};

use crate::support::{TestClient, TestServer, mkv_capabilities};

fn episode(key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, MediaKind::Episode, key).unwrap()
}

/// A server with alice, both episodes written, and a client signed in as
/// her.
async fn ready(test: &str) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start(test).await;
    server.write_episode("12");
    server.write_episode("13");
    server.add_user("alice", "correct horse", None).await;
    let client = signed_in(&server, "alice", "correct horse").await;
    (server, client.0, client.1)
}

async fn signed_in(server: &TestServer, user: &str, password: &str) -> (TestClient, SessionId) {
    let client = server.client().await;
    client.hello_with(None, Some(mkv_capabilities())).await;
    let session = client.login(user, password).await;
    (client, session)
}

/// The tracks the server chooses for `id`, with no selection.
async fn chosen(client: &TestClient, session: &SessionId, id: &MediaId) -> ChosenTracks {
    let request = Request::PlaybackPlan {
        id: id.clone(),
        selection: None,
        bitmap: Default::default(),
    };
    match client.call(Some(session), request).await {
        Response::Ok(Reply::PlaybackPlan {
            chosen: Some(chosen),
            ..
        }) => chosen,
        other => panic!("no choice: {other:?}"),
    }
}

async fn done(client: &TestClient, session: &SessionId, request: Request) {
    assert_eq!(
        client.call(Some(session), request).await,
        Response::Ok(Reply::Done)
    );
}

fn always_english() -> TrackPreferences {
    let mut p = TrackPreferences::default();
    p.audio.languages = vec!["eng".into()];
    p.subtitles.mode = SubtitleMode::Always;
    p.subtitles.languages = vec!["eng".into()];
    p.subtitles.sdh = SdhPreference::Accept;
    p
}

/// Position of the SDH subtitle in the B99 fixture.
async fn sdh(client: &TestClient, session: &SessionId) -> u32 {
    let Response::Ok(Reply::Tracks(tracks)) = client
        .call(Some(session), Request::Tracks { id: episode("12") })
        .await
    else {
        panic!("no tracks");
    };
    tracks
        .subtitles
        .iter()
        .position(|s| s.role == SubtitleRole::Sdh)
        .unwrap() as u32
}

#[tokio::test]
async fn preferences_set_on_one_device_choose_on_another() {
    let (server, laptop, on_laptop) = ready("tracks-two-devices").await;
    done(
        &laptop,
        &on_laptop,
        Request::SetTrackPreferences {
            preferences: always_english(),
        },
    )
    .await;

    let (tv, on_tv) = signed_in(&server, "alice", "correct horse").await;
    assert_eq!(
        tv.call(Some(&on_tv), Request::GetTrackPreferences).await,
        Response::Ok(Reply::TrackPreferences(always_english()))
    );
    let chosen = chosen(&tv, &on_tv, &episode("12")).await;
    assert_eq!(chosen.selection.subtitle, Some(sdh(&tv, &on_tv).await));
    assert_eq!(chosen.subtitle, TrackReason::Preferences);
    laptop.close().await;
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_explicit_selection_is_used_as_sent() {
    let (server, client, alice) = ready("tracks-explicit").await;
    let request = Request::PlaybackPlan {
        id: episode("12"),
        selection: Some(Selection {
            audio: Some(1),
            subtitle: None,
        }),
        bitmap: Default::default(),
    };
    let Response::Ok(Reply::PlaybackPlan { chosen, .. }) = client.call(Some(&alice), request).await
    else {
        panic!("no plan");
    };
    assert_eq!(chosen, None);
    client.close().await;
    server.stop().await;
}
