//! Track preferences and the server's choice of tracks (P1-20) over a real
//! Iroh connection: that preferences reach the plan, and that a sent
//! selection is used as it is. The choosing itself is tested in
//! `tracks::tests`. Episodes 12 and 13 are probed as Brooklyn Nine-Nine
//! S07E01: English audio description before the main English 5.1, and an
//! "SDH" subtitle as the only English one.

use spoolframe_protocol::{
    ChosenTracks, Reply, Request, Response, Selection, SessionId, TrackPreferencesInfo, TrackReason,
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
        display: None,
    };
    match client.call(Some(session), request).await {
        Response::Ok(Reply::PlaybackPlan {
            chosen: Some(chosen),
            ..
        }) => chosen,
        other => panic!("no choice: {other:?}"),
    }
}

/// A client signed in as alice, whose `Hello` says `language`.
async fn speaking(server: &TestServer, language: Option<&str>) -> (TestClient, SessionId) {
    let client = server.client().await;
    client
        .hello_speaking(mkv_capabilities(), &[], language)
        .await;
    let session = client.login("alice", "correct horse").await;
    (client, session)
}

/// The language of the audio track chosen for episode 12.
async fn heard(client: &TestClient, session: &SessionId) -> (Option<String>, TrackReason) {
    let chosen = chosen(client, session, &episode("12")).await;
    let Response::Ok(Reply::Tracks(tracks)) = client
        .call(Some(session), Request::Tracks { id: episode("12") })
        .await
    else {
        panic!("no tracks");
    };
    let picked = chosen.selection.audio.unwrap() as usize;
    (tracks.audio[picked].language.clone(), chosen.audio)
}

#[tokio::test]
async fn with_nothing_saved_the_device_s_language_chooses_the_audio() {
    let (server, _, _) = ready("tracks-device-language").await;
    let (tv, on_tv) = speaking(&server, Some("de-DE")).await;
    assert_eq!(
        heard(&tv, &on_tv).await,
        (Some("ger".into()), TrackReason::Preferences)
    );
    tv.close().await;
}

/// `ClientOptions::language` reaches `Hello`.
#[tokio::test]
async fn an_app_s_language_is_said_by_the_sdk() {
    use std::sync::Arc;

    use spoolframe_client_sdk::{Client, ClientOptions, Device, MemoryStorage};

    let (server, _, _) = ready("tracks-sdk-language").await;
    let options = ClientOptions {
        lan_discovery: false,
        language: Some("de-DE".into()),
        ..ClientOptions::default()
    };
    let device = Device {
        name: "TV".into(),
        client_name: "sdk-test".into(),
        client_version: "0".into(),
        platform: spoolframe_client_sdk::types::Platform::Android,
        capabilities: mkv_capabilities(),
    };
    let client = Client::open_with_options(Arc::new(MemoryStorage::default()), device, options)
        .await
        .unwrap();
    let session = crate::federation::app::sign_in(&client, &server, "alice").await;
    let plan = session.playback_plan(&episode("12"), None).await.unwrap();
    assert_eq!(plan.chosen.map(|c| c.audio), Some(TrackReason::Preferences));
    client.close().await;
}

#[tokio::test]
async fn an_older_client_that_says_no_language_gets_the_file_s_default() {
    let (server, _, _) = ready("tracks-no-language").await;
    let (tv, on_tv) = speaking(&server, None).await;
    assert_eq!(
        heard(&tv, &on_tv).await,
        (Some("eng".into()), TrackReason::FileDefault)
    );
    tv.close().await;
}

/// Saved with no language at all: the viewer asked for the file's default,
/// and the device has no say.
#[tokio::test]
async fn a_saved_row_wins_over_the_device_s_language() {
    let (server, _, _) = ready("tracks-saved-wins").await;
    let (tv, on_tv) = speaking(&server, Some("de-DE")).await;
    done(
        &tv,
        &on_tv,
        Request::SetTrackPreferences {
            preferences: TrackPreferences::default(),
        },
    )
    .await;
    assert_eq!(
        heard(&tv, &on_tv).await,
        (Some("eng".into()), TrackReason::FileDefault)
    );
    tv.close().await;
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
        Response::Ok(Reply::TrackPreferences(TrackPreferencesInfo {
            preferences: always_english(),
            ocr_offered: false,
        }))
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
        display: None,
    };
    let Response::Ok(Reply::PlaybackPlan { chosen, .. }) = client.call(Some(&alice), request).await
    else {
        panic!("no plan");
    };
    assert_eq!(chosen, None);
    client.close().await;
    server.stop().await;
}
