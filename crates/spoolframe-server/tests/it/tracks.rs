//! Track preferences and the server's choice of tracks (P1-20) over a real
//! Iroh connection. Episodes 12 and 13 are probed as Brooklyn Nine-Nine
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
async fn a_new_user_gets_the_main_audio_and_no_description() {
    let (server, client, alice) = ready("tracks-defaults").await;
    let chosen = chosen(&client, &alice, &episode("12")).await;
    assert_eq!(chosen.selection.audio, Some(2));
    assert_eq!(chosen.audio, TrackReason::FileDefault);
    // Forced only, and B99 has no English forced subtitle.
    assert_eq!(chosen.selection.subtitle, None);
    assert_eq!(chosen.subtitle, TrackReason::NoneMatched);
    client.close().await;
    server.stop().await;
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
async fn preferences_are_each_user_s_own() {
    let (server, client, alice) = ready("tracks-per-user").await;
    server.add_user("bob", "battery staple", None).await;
    let bob = client.login("bob", "battery staple").await;
    done(
        &client,
        &alice,
        Request::SetTrackPreferences {
            preferences: always_english(),
        },
    )
    .await;
    assert_eq!(
        client.call(Some(&bob), Request::GetTrackPreferences).await,
        Response::Ok(Reply::TrackPreferences(TrackPreferences::default()))
    );
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_pick_on_one_episode_carries_to_the_next_until_cleared() {
    let (server, client, alice) = ready("tracks-override").await;
    done(
        &client,
        &alice,
        Request::SetTrackPreferences {
            preferences: always_english(),
        },
    )
    .await;
    // On episode 12 the viewer turns subtitles off.
    done(
        &client,
        &alice,
        Request::SetTrackOverride {
            id: episode("12"),
            selection: Selection {
                audio: Some(2),
                subtitle: None,
            },
        },
    )
    .await;
    let next = chosen(&client, &alice, &episode("13")).await;
    assert_eq!(next.selection.subtitle, None);
    assert_eq!(next.subtitle, TrackReason::Override);
    assert_eq!(next.audio, TrackReason::Override);

    done(
        &client,
        &alice,
        Request::ClearTrackOverride { id: episode("13") },
    )
    .await;
    let after = chosen(&client, &alice, &episode("13")).await;
    assert_eq!(after.subtitle, TrackReason::Preferences);
    client.close().await;
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
