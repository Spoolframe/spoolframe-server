//! "My profile" (P3-03): what a user changes about their own account in
//! the web UI, over `me.*`, and what their devices then see.

use std::net::SocketAddr;

use jsonrpsee::core::client::ClientT;
use jsonrpsee::core::params::ObjectParams;
use jsonrpsee::ws_client::WsClient;
use spoolframe_admin_api::{
    MeApiClient, SubtitlePick, TrackPreferencesInfo as AdminTrackPreferences, error_code,
};
use spoolframe_protocol::{
    ChosenTracks, Error, Reply, Request, Response, Selection, SessionId, TrackPreferencesInfo,
    TrackReason, UserSummary,
};
use spoolframe_types::{
    MediaId, MediaKind, Namespace, SdhPreference, SubtitleMode, SubtitleRole, TrackPreferences,
};

use crate::admin::{anonymous, code, creds, serve_beside, with_token};
use crate::support::{TestClient, TestServer, mkv_capabilities};

const WEB: &str = "https://media.example.org";
const PASSWORD: &str = "correct horse";

/// A server with alice and bob, and its web UI's API.
async fn server(test: &str) -> (TestServer, SocketAddr) {
    let server = TestServer::start(test).await;
    server.add_user("alice", PASSWORD, None).await;
    server.add_user("bob", PASSWORD, None).await;
    let web = serve_beside(&server, WEB).await;
    (server, web)
}

/// `user` signed in to the web UI.
async fn on_the_web(web: SocketAddr, user: &str) -> WsClient {
    let login = anonymous(web)
        .await
        .login(creds(user, PASSWORD))
        .await
        .unwrap();
    with_token(web, &login.token).await
}

/// `user` signed in on a device of their own, as the Linux client is.
async fn on_a_device(server: &TestServer, user: &str) -> (TestClient, SessionId) {
    let client = server.client().await;
    client
        .hello_with(Some("Living room PC"), Some(mkv_capabilities()))
        .await;
    let session = client.login(user, PASSWORD).await;
    (client, session)
}

async fn who(client: &TestClient, session: &SessionId) -> Response {
    client.call(Some(session), Request::WhoAmI).await
}

fn shown_as(response: Response) -> String {
    match response {
        Response::Ok(Reply::WhoAmI(UserSummary { display_name, .. })) => display_name,
        other => panic!("not WhoAmI: {other:?}"),
    }
}

#[tokio::test]
async fn a_display_name_set_on_the_web_is_what_the_app_shows() {
    let (server, web) = server("display-name").await;
    let (pc, session) = on_a_device(&server, "alice").await;
    let alice = on_the_web(web, "alice").await;
    assert_eq!(shown_as(who(&pc, &session).await), "alice");

    let me = alice.set_display_name(Some(" Ally ".into())).await.unwrap();
    assert_eq!(me.display_name.as_deref(), Some("Ally"));
    assert_eq!(shown_as(who(&pc, &session).await), "Ally");

    let long = alice.set_display_name(Some("x".repeat(65))).await;
    assert_eq!(code(long.unwrap_err()), error_code::INVALID_INPUT);
    assert_eq!(shown_as(who(&pc, &session).await), "Ally");

    // Blank: the username again.
    alice.set_display_name(None).await.unwrap();
    assert_eq!(shown_as(who(&pc, &session).await), "alice");
    pc.close().await;
    server.stop().await;
}

#[tokio::test]
async fn revoking_a_device_signs_it_out() {
    let (server, web) = server("revoke-device").await;
    let (pc, on_pc) = on_a_device(&server, "alice").await;
    let alice = on_the_web(web, "alice").await;
    let sessions = alice.sessions().await.unwrap();
    let device = sessions
        .iter()
        .find(|s| s.endpoint_id.is_some())
        .expect("the PC's session");
    assert_eq!(device.device_name.as_deref(), Some("Living room PC"));
    assert!(!device.current);

    alice.revoke_session(device.id.clone()).await.unwrap();
    // Its connection is closed (PX-10); back, its session is refused.
    pc.conn.closed().await;
    let pc = pc.reconnect(&server).await;
    assert_eq!(
        who(&pc, &on_pc).await,
        Response::Err(Error::Unauthenticated)
    );
    // The web session that did it goes on.
    alice.who_am_i().await.unwrap();
    pc.close().await;
    server.stop().await;
}

#[tokio::test]
async fn signing_out_everywhere_else_leaves_only_this_web_session() {
    let (server, web) = server("everywhere-else").await;
    let (pc, on_pc) = on_a_device(&server, "alice").await;
    let laptop = on_the_web(web, "alice").await;
    let phone = on_the_web(web, "alice").await;
    let (bobs_pc, on_bobs_pc) = on_a_device(&server, "bob").await;

    assert_eq!(phone.revoke_other_sessions().await.unwrap(), 2);
    let left = phone.sessions().await.unwrap();
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(left[0].current);
    // Its connection is closed (PX-10); back, its session is refused.
    pc.conn.closed().await;
    let pc = pc.reconnect(&server).await;
    assert_eq!(
        who(&pc, &on_pc).await,
        Response::Err(Error::Unauthenticated)
    );
    assert_eq!(
        code(laptop.who_am_i().await.unwrap_err()),
        error_code::UNAUTHENTICATED
    );
    // Bob is not signed out of anything.
    assert_eq!(shown_as(who(&bobs_pc, &on_bobs_pc).await), "bob");
    pc.close().await;
    bobs_pc.close().await;
    server.stop().await;
}

fn episode(key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, MediaKind::Episode, key).unwrap()
}

/// The tracks the server chooses for `id` on the device, with no
/// selection: what its next playback uses.
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

fn english_with_subtitles() -> TrackPreferences {
    let mut p = TrackPreferences::default();
    p.audio.languages = vec!["eng".into()];
    p.subtitles.mode = SubtitleMode::Always;
    p.subtitles.languages = vec!["eng".into()];
    p.subtitles.sdh = SdhPreference::Accept;
    p
}

/// Episodes 12 and 13 are probed as Brooklyn Nine-Nine S07E01, whose only
/// English subtitle is SDH (P1-20's fixture).
#[tokio::test]
async fn preferences_set_on_the_web_choose_the_app_s_next_playback() {
    let (server, web) = server("preferences").await;
    server.write_episode("12");
    server.write_episode("13");
    let (pc, on_pc) = on_a_device(&server, "alice").await;
    let alice = on_the_web(web, "alice").await;
    assert_eq!(
        alice.track_preferences().await.unwrap(),
        AdminTrackPreferences {
            preferences: TrackPreferences::default(),
            ocr_offered: false,
        }
    );
    // Forced only by default, and the episode has no forced subtitle.
    assert_eq!(
        chosen(&pc, &on_pc, &episode("12")).await.subtitle,
        TrackReason::NoneMatched
    );

    alice
        .set_track_preferences(english_with_subtitles())
        .await
        .unwrap();
    assert_eq!(
        pc.call(Some(&on_pc), Request::GetTrackPreferences).await,
        Response::Ok(Reply::TrackPreferences(TrackPreferencesInfo {
            preferences: english_with_subtitles(),
            ocr_offered: false,
        }))
    );
    let next = chosen(&pc, &on_pc, &episode("12")).await;
    assert_eq!(next.subtitle, TrackReason::Preferences);
    assert!(next.selection.subtitle.is_some());
    pc.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_series_override_made_in_the_app_is_listed_and_reset_on_the_web() {
    let (server, web) = server("overrides").await;
    server.write_episode("12");
    server.write_episode("13");
    let (pc, on_pc) = on_a_device(&server, "alice").await;
    let alice = on_the_web(web, "alice").await;
    // Subtitles off for the rest of the series.
    assert_eq!(
        pc.call(
            Some(&on_pc),
            Request::SetTrackOverride {
                id: episode("12"),
                selection: Selection {
                    audio: Some(2),
                    subtitle: None,
                },
            },
        )
        .await,
        Response::Ok(Reply::Done)
    );
    alice
        .set_track_preferences(english_with_subtitles())
        .await
        .unwrap();
    assert_eq!(
        chosen(&pc, &on_pc, &episode("13")).await.subtitle,
        TrackReason::Override
    );

    let overrides = alice.track_overrides().await.unwrap();
    assert_eq!(overrides.len(), 1, "{overrides:?}");
    let show = &overrides[0];
    assert_eq!(show.id, "plex:show:10");
    assert_eq!(show.subtitle, Some(SubtitlePick::Off));
    assert_eq!(
        show.audio.as_ref().unwrap().language.as_deref(),
        Some("eng")
    );

    alice.clear_track_override(show.id.clone()).await.unwrap();
    assert_eq!(alice.track_overrides().await.unwrap(), []);
    let next = chosen(&pc, &on_pc, &episode("13")).await;
    assert_eq!(next.subtitle, TrackReason::Preferences);
    let sdh = match pc
        .call(Some(&on_pc), Request::Tracks { id: episode("13") })
        .await
    {
        Response::Ok(Reply::Tracks(tracks)) => {
            tracks.subtitles[next.selection.subtitle.unwrap() as usize].role
        }
        other => panic!("no tracks: {other:?}"),
    };
    assert_eq!(sdh, SubtitleRole::Sdh);
    pc.close().await;
    server.stop().await;
}

/// Each `me.*` method acts on the caller only: a request naming someone
/// else, by a field the method does not take or by an id of theirs, does
/// nothing to them.
#[tokio::test]
async fn no_me_method_reaches_another_user_s_account() {
    let (server, web) = server("only-me").await;
    server.write_episode("12");
    let (bobs_pc, on_bobs_pc) = on_a_device(&server, "bob").await;
    assert_eq!(
        bobs_pc
            .call(
                Some(&on_bobs_pc),
                Request::SetTrackOverride {
                    id: episode("12"),
                    selection: Selection {
                        audio: None,
                        subtitle: None,
                    },
                },
            )
            .await,
        Response::Ok(Reply::Done)
    );
    let bob = on_the_web(web, "bob").await;
    let alice = on_the_web(web, "alice").await;
    let bobs_session = bob
        .sessions()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.endpoint_id.is_some())
        .unwrap();

    // A hand-made call with a username the method does not take.
    let mut params = ObjectParams::new();
    params.insert("displayName", "Mallory").unwrap();
    params.insert("username", "bob").unwrap();
    let _: serde_json::Value = alice
        .request("me.setDisplayName", params)
        .await
        .unwrap_or_default();
    assert_eq!(bob.who_am_i().await.unwrap().display_name, None);
    assert_eq!(shown_as(who(&bobs_pc, &on_bobs_pc).await), "bob");

    // Bob's session, device and override, by their ids.
    let refused = alice.revoke_session(bobs_session.id.clone()).await;
    assert_eq!(code(refused.unwrap_err()), error_code::NO_SUCH_SESSION);
    let device = bobs_session.endpoint_id.clone().unwrap();
    let refused = alice.set_pin_required(device, true).await;
    assert_eq!(code(refused.unwrap_err()), error_code::NO_SUCH_DEVICE);
    alice
        .clear_track_override("plex:show:10".into())
        .await
        .unwrap();
    assert_eq!(bob.track_overrides().await.unwrap().len(), 1);
    alice
        .set_track_preferences(english_with_subtitles())
        .await
        .unwrap();
    assert_eq!(
        bob.track_preferences().await.unwrap().preferences,
        TrackPreferences::default()
    );
    alice.revoke_other_sessions().await.unwrap();
    assert_eq!(shown_as(who(&bobs_pc, &on_bobs_pc).await), "bob");
    bob.who_am_i().await.unwrap();
    bobs_pc.close().await;
    server.stop().await;
}

/// Whether `web`'s session still works: `whoAmI`, or the error's code.
async fn web_works(web: &WsClient) -> Result<(), i32> {
    web.who_am_i().await.map(|_| ()).map_err(code)
}

/// Whether `pc`'s connection was closed as signed out (PX-10) within 5 s;
/// then the device back on a new connection.
async fn signed_out(server: &TestServer, pc: TestClient) -> TestClient {
    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), pc.conn.closed())
        .await
        .expect("the device's connection was not closed");
    assert!(
        matches!(
            &closed,
            iroh::endpoint::ConnectionError::ApplicationClosed(close)
                if close.error_code
                    == iroh::endpoint::VarInt::from_u32(spoolframe_protocol::close::SIGNED_OUT)
        ),
        "{closed:?}"
    );
    pc.reconnect(server).await
}

/// PX-09 finding 4 (A-2): a new password signs out every other session of
/// its user, on the web and on devices; the one that changed it stays.
#[tokio::test]
async fn a_password_change_signs_out_every_other_session() {
    let (server, web) = server("password-revokes").await;
    let (pc, session) = on_a_device(&server, "alice").await;
    let elsewhere = on_the_web(web, "alice").await;
    let here = on_the_web(web, "alice").await;
    let (bob_pc, bob_session) = on_a_device(&server, "bob").await;

    here.set_password(spoolframe_admin_api::PasswordChange {
        current_password: PASSWORD.into(),
        new_password: "battery staple".into(),
    })
    .await
    .unwrap();

    // Its connection is closed at once (PX-10).
    let pc = signed_out(&server, pc).await;
    assert_eq!(
        who(&pc, &session).await,
        Response::Err(Error::Unauthenticated)
    );
    assert_eq!(
        web_works(&elsewhere).await,
        Err(error_code::UNAUTHENTICATED)
    );
    assert_eq!(web_works(&here).await, Ok(()));
    // Nobody else's.
    assert_eq!(shown_as(who(&bob_pc, &bob_session).await), "bob");
    pc.close().await;
    bob_pc.close().await;
    server.stop().await;
}

/// An admin `root` beside alice and bob, signed in to the web UI.
async fn as_root(server: &TestServer, web: SocketAddr) -> WsClient {
    server.add_user("root", PASSWORD, None).await;
    server
        .store
        .set_role("root", spoolframe_types::Role::Admin)
        .await
        .unwrap();
    on_the_web(web, "root").await
}

/// An admin's reset signs the user out everywhere: whoever knew the old
/// password is out.
#[tokio::test]
async fn an_admin_reset_signs_the_user_out_everywhere() {
    use spoolframe_admin_api::AdminApiClient;
    let (server, web) = server("reset-revokes").await;
    let root = as_root(&server, web).await;
    let (pc, session) = on_a_device(&server, "alice").await;
    let alice = on_the_web(web, "alice").await;

    root.user_set_password(creds("alice", "battery staple"))
        .await
        .unwrap();

    // Its connection is closed at once (PX-10).
    let pc = signed_out(&server, pc).await;
    assert_eq!(
        who(&pc, &session).await,
        Response::Err(Error::Unauthenticated)
    );
    assert_eq!(web_works(&alice).await, Err(error_code::UNAUTHENTICATED));
    assert_eq!(web_works(&root).await, Ok(()));
    pc.close().await;
    server.stop().await;
}

/// An admin resetting their own password keeps the session they did it
/// from, as a change of their own does.
#[tokio::test]
async fn an_admin_resetting_their_own_password_keeps_that_session() {
    use spoolframe_admin_api::AdminApiClient;
    let (server, web) = server("reset-self-revokes").await;
    let root = as_root(&server, web).await;
    let other = on_the_web(web, "root").await;
    let (pc, session) = on_a_device(&server, "root").await;

    root.user_set_password(creds("root", "battery staple"))
        .await
        .unwrap();

    assert_eq!(web_works(&root).await, Ok(()));
    assert_eq!(web_works(&other).await, Err(error_code::UNAUTHENTICATED));
    // Its connection is closed at once (PX-10).
    let pc = signed_out(&server, pc).await;
    assert_eq!(
        who(&pc, &session).await,
        Response::Err(Error::Unauthenticated)
    );
    pc.close().await;
    server.stop().await;
}
