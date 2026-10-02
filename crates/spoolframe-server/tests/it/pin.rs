//! Profile PINs (P2-08): required per device, enforced by the server on
//! every stream of a locked session.

use crate::support;

use std::time::Duration;

use spoolframe_protocol::{
    ArtworkResponse, ArtworkSize, Error, MediaReadResponse, Reply, Request, Response, Selection,
    SessionId,
};
use support::{ServerOptions, TestClient, TestServer, fresh_dir, mkv_capabilities, movie, poster};

const PASSWORD: &str = "correct horse";
const PIN: &str = "4321";

/// Alice signed in on a device whose capabilities play the stand-in movie.
async fn alice_on(server: &TestServer) -> (TestClient, SessionId) {
    let client = server.client().await;
    client.hello_with(None, Some(mkv_capabilities())).await;
    let session = client.login("alice", PASSWORD).await;
    (client, session)
}

async fn ok(client: &TestClient, session: &SessionId, request: Request) -> Reply {
    match client.call(Some(session), request).await {
        Response::Ok(reply) => reply,
        Response::Err(e) => panic!("refused: {e:?}"),
    }
}

async fn err(client: &TestClient, session: &SessionId, request: Request) -> Error {
    match client.call(Some(session), request).await {
        Response::Err(e) => e,
        Response::Ok(reply) => panic!("answered: {reply:?}"),
    }
}

/// A server with alice, who has a PIN and requires it on her first
/// device, and that device reconnected (an app restart): its session is
/// locked there.
async fn locked(test: &str, options: ServerOptions) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start_with(fresh_dir(test).await, options).await;
    server.write_media(1000);
    server.add_user("alice", PASSWORD, None).await;
    let (tv, session) = alice_on(&server).await;
    ok(&tv, &session, Request::SetPin { pin: PIN.into() }).await;
    ok(
        &tv,
        &session,
        Request::SetPinRequired {
            device: None,
            required: true,
        },
    )
    .await;
    // Turning it on here does not lock this connection out mid-use.
    ok(&tv, &session, Request::Libraries).await;
    let restarted = tv.reconnect(&server).await;
    (server, restarted, session)
}

async fn who(client: &TestClient, session: &SessionId) -> bool {
    match ok(client, session, Request::WhoAmI).await {
        Reply::WhoAmI(user) => user.locked,
        other => panic!("not WhoAmI: {other:?}"),
    }
}

#[tokio::test]
async fn a_required_pin_locks_the_device_until_it_is_entered() {
    let (server, tv, session) = locked("pin-lock", ServerOptions::default()).await;
    assert!(who(&tv, &session).await);
    assert_eq!(
        err(&tv, &session, Request::Libraries).await,
        Error::ProfileLocked
    );
    assert_eq!(
        err(&tv, &session, Request::UnlockProfile { pin: "0000".into() }).await,
        Error::InvalidCredentials
    );
    ok(&tv, &session, Request::UnlockProfile { pin: PIN.into() }).await;
    assert!(!who(&tv, &session).await);
    ok(&tv, &session, Request::Libraries).await;

    // Another of alice's devices does not ask.
    let (desktop, other) = alice_on(&server).await;
    assert!(!who(&desktop, &other).await);
    ok(&desktop, &other, Request::Libraries).await;
    tv.close().await;
    desktop.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_locked_session_gets_nothing_on_any_stream() {
    let server =
        TestServer::start_with(fresh_dir("pin-streams").await, ServerOptions::default()).await;
    server.write_media(1000);
    server.add_user("alice", PASSWORD, None).await;
    let (tv, session) = alice_on(&server).await;
    // A playback started before the PIN is required on this device.
    let handle = match ok(
        &tv,
        &session,
        Request::StartPlayback {
            id: movie().id().clone(),
            selection: Some(Selection::default()),
            start_ms: None,
            bitmap: Default::default(),
        },
    )
    .await
    {
        Reply::PlaybackStarted { handle, .. } => handle,
        other => panic!("not started: {other:?}"),
    };
    ok(&tv, &session, Request::SetPin { pin: PIN.into() }).await;
    ok(
        &tv,
        &session,
        Request::SetPinRequired {
            device: None,
            required: true,
        },
    )
    .await;
    let tv = tv.reconnect(&server).await;

    // A client that skips its own PIN screen gets nothing.
    assert_eq!(
        err(&tv, &session, Request::Libraries).await,
        Error::ProfileLocked
    );
    assert_eq!(
        err(
            &tv,
            &session,
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: Some(Selection::default()),
                start_ms: None,
                bitmap: Default::default(),
            }
        )
        .await,
        Error::ProfileLocked
    );
    let (read, bytes) = tv.read_at(&session, &handle, 0, 0, 100).await;
    assert_eq!(read, MediaReadResponse::Err(Error::ProfileLocked));
    assert!(bytes.is_empty());
    let (art, bytes) = tv.artwork(&session, poster(), ArtworkSize::Small).await;
    assert_eq!(art, ArtworkResponse::Err(Error::ProfileLocked));
    assert!(bytes.is_empty());

    // Unlocked, the same playback reads.
    ok(&tv, &session, Request::UnlockProfile { pin: PIN.into() }).await;
    let (read, _) = tv.read_at(&session, &handle, 0, 0, 100).await;
    assert!(matches!(read, MediaReadResponse::Ok(_)), "{read:?}");
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn switching_to_another_profile_and_back_asks_again() {
    let (server, tv, alice) = locked("pin-switch", ServerOptions::default()).await;
    server.add_user("bob", "battery staple", None).await;
    let bob = tv.login("bob", "battery staple").await;
    ok(&tv, &alice, Request::UnlockProfile { pin: PIN.into() }).await;
    ok(&tv, &alice, Request::Libraries).await;
    // "Who's watching" asks every profile who it is: that is no switch.
    who(&tv, &bob).await;
    ok(&tv, &alice, Request::Libraries).await;
    // Bob watches; back to alice, her PIN again.
    ok(&tv, &bob, Request::Libraries).await;
    assert_eq!(
        err(&tv, &alice, Request::Libraries).await,
        Error::ProfileLocked
    );
    tv.close().await;
    server.stop().await;
}

/// A PIN only locks and unlocks a session a real sign-in made, so an
/// unlocked session changes it and its switch with nothing more; a locked
/// one cannot touch either (P2-08).
#[tokio::test]
async fn only_an_unlocked_session_changes_the_pin_or_its_switch() {
    let (server, tv, session) = locked("pin-off", ServerOptions::default()).await;
    let off = Request::SetPinRequired {
        device: None,
        required: false,
    };
    assert_eq!(err(&tv, &session, off.clone()).await, Error::ProfileLocked);
    assert_eq!(
        err(&tv, &session, Request::ClearPin).await,
        Error::ProfileLocked
    );
    assert_eq!(
        err(&tv, &session, Request::SetPin { pin: "0000".into() }).await,
        Error::ProfileLocked
    );
    ok(&tv, &session, Request::UnlockProfile { pin: PIN.into() }).await;
    ok(&tv, &session, off).await;
    let tv = tv.reconnect(&server).await;
    assert!(!who(&tv, &session).await);
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_pin_is_four_to_eight_digits() {
    let server =
        TestServer::start_with(fresh_dir("pin-format").await, ServerOptions::default()).await;
    server.add_user("alice", PASSWORD, None).await;
    let (tv, session) = alice_on(&server).await;
    for bad in ["123", "123456789", "12a4"] {
        let refused = err(&tv, &session, Request::SetPin { pin: bad.into() }).await;
        assert!(
            matches!(refused, Error::InvalidInput { .. }),
            "{bad}: {refused:?}"
        );
    }
    // Requiring a PIN that is not there is refused.
    let refused = err(
        &tv,
        &session,
        Request::SetPinRequired {
            device: None,
            required: true,
        },
    )
    .await;
    assert!(matches!(refused, Error::InvalidInput { .. }), "{refused:?}");
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn ten_wrong_pins_revoke_that_device_s_session_only() {
    // Failures count for a moment only, so ten fit in a test.
    let options = ServerOptions {
        failure_window: Duration::from_millis(200),
        ..ServerOptions::default()
    };
    let (server, tv, session) = locked("pin-revoke", options).await;
    let (desktop, other) = alice_on(&server).await;
    let mut wrong = 0;
    let last = loop {
        match tv
            .call(
                Some(&session),
                Request::UnlockProfile { pin: "0000".into() },
            )
            .await
        {
            Response::Err(Error::RateLimited { .. }) => {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Response::Err(Error::InvalidCredentials) => wrong += 1,
            Response::Err(e) => break e,
            Response::Ok(reply) => panic!("unlocked by a wrong PIN: {reply:?}"),
        }
    };
    assert_eq!(wrong, 9);
    assert_eq!(last, Error::Unauthenticated);
    // A full sign-in is needed there; even the right PIN does nothing.
    assert_eq!(
        err(&tv, &session, Request::UnlockProfile { pin: PIN.into() }).await,
        Error::Unauthenticated
    );
    // The other device is untouched.
    ok(&desktop, &other, Request::Libraries).await;
    tv.close().await;
    desktop.close().await;
    server.stop().await;
}

#[tokio::test]
async fn devices_are_listed_and_set_from_elsewhere() {
    let server =
        TestServer::start_with(fresh_dir("pin-devices").await, ServerOptions::default()).await;
    server.add_user("alice", PASSWORD, None).await;
    let (tv, on_tv) = alice_on(&server).await;
    let (desktop, on_desktop) = alice_on(&server).await;
    ok(&desktop, &on_desktop, Request::SetPin { pin: PIN.into() }).await;
    let Reply::Devices { devices } = ok(&desktop, &on_desktop, Request::ListDevices).await else {
        panic!("no devices");
    };
    assert_eq!(devices.len(), 2);
    assert_eq!(devices.iter().filter(|d| d.this_device).count(), 1);
    let tv_id = tv.endpoint.id().to_string();
    assert!(
        devices
            .iter()
            .any(|d| d.endpoint_id == tv_id && !d.pin_required)
    );
    // From the desktop, for the TV, with its awkward keyboard.
    ok(
        &desktop,
        &on_desktop,
        Request::SetPinRequired {
            device: Some(tv_id.clone()),
            required: true,
        },
    )
    .await;
    let tv = tv.reconnect(&server).await;
    assert!(who(&tv, &on_tv).await);
    // And the desktop, where it is not required, is not locked.
    let desktop = desktop.reconnect(&server).await;
    assert!(!who(&desktop, &on_desktop).await);
    tv.close().await;
    desktop.close().await;
    server.stop().await;
}
