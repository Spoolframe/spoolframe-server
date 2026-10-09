//! Profile PINs (P2-08): required per device, enforced by the server on
//! every stream of a locked session. What runs of wrong PINs and
//! passwords do to sessions is tested on `Auth` in `auth::tests::pins`;
//! these check which request counts against which limit.

use crate::support;

use std::time::Duration;

use spoolframe_protocol::{
    ArtworkResponse, ArtworkSize, Error, LoginPassword, MediaReadResponse, PinProof, PinState,
    Reply, Request, Response, Selection, SessionId,
};
use spoolframe_server::auth::SessionToken;
use spoolframe_server::store::passkeys::FirstPasskey;
use spoolframe_server::store::sessions::SessionBinding;
use spoolframe_types::Platform;
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
    ok(
        &tv,
        &session,
        Request::SetPin {
            pin: PIN.into(),
            proof: None,
        },
    )
    .await;
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
            quality: Default::default(),
        },
    )
    .await
    {
        Reply::PlaybackStarted { handle, .. } => handle,
        other => panic!("not started: {other:?}"),
    };
    ok(
        &tv,
        &session,
        Request::SetPin {
            pin: PIN.into(),
            proof: None,
        },
    )
    .await;
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
                quality: Default::default(),
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
    ok(&tv, &alice, Request::ActivateProfile).await;
    ok(&tv, &alice, Request::UnlockProfile { pin: PIN.into() }).await;
    ok(&tv, &alice, Request::Libraries).await;
    // "Who's watching" asks every profile who it is: that is no switch.
    who(&tv, &bob).await;
    ok(&tv, &alice, Request::Libraries).await;
    // Background requests on Bob do not switch the foreground profile.
    ok(&tv, &bob, Request::Libraries).await;
    ok(&tv, &alice, Request::Libraries).await;
    // Only explicit selection relocks Alice.
    ok(&tv, &bob, Request::ActivateProfile).await;
    ok(&tv, &alice, Request::ActivateProfile).await;
    assert_eq!(
        err(&tv, &alice, Request::Libraries).await,
        Error::ProfileLocked
    );
    tv.close().await;
    server.stop().await;
}

/// A locked session cannot touch the PIN or its switch, not even with
/// the PIN: it unlocks first (P2-08).
#[tokio::test]
async fn only_an_unlocked_session_changes_the_pin_or_its_switch() {
    let (server, tv, session) = locked("pin-off", ServerOptions::default()).await;
    let off = Request::SetPinRequired {
        device: None,
        required: false,
    };
    assert_eq!(err(&tv, &session, off.clone()).await, Error::ProfileLocked);
    assert_eq!(
        err(
            &tv,
            &session,
            Request::ClearPin {
                proof: PinProof::Pin { pin: PIN.into() }
            }
        )
        .await,
        Error::ProfileLocked
    );
    assert_eq!(
        err(
            &tv,
            &session,
            Request::SetPin {
                pin: "0000".into(),
                proof: Some(PinProof::Pin { pin: PIN.into() })
            }
        )
        .await,
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
        let refused = err(
            &tv,
            &session,
            Request::SetPin {
                pin: bad.into(),
                proof: None,
            },
        )
        .await;
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
                server.clock.advance(Duration::from_millis(200));
            }
            Response::Err(Error::InvalidCredentials) => wrong += 1,
            Response::Err(e) => break e,
            Response::Ok(reply) => panic!("unlocked by a wrong PIN: {reply:?}"),
        }
    };
    assert_eq!(wrong, 9);
    assert_eq!(last, Error::Unauthenticated);
    // Its last session gone, the TV's connection is closed (PX-10).
    tv.conn.closed().await;
    let tv = tv.reconnect(&server).await;
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
    ok(
        &desktop,
        &on_desktop,
        Request::SetPin {
            pin: PIN.into(),
            proof: None,
        },
    )
    .await;
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

async fn pin_state(client: &TestClient, session: &SessionId) -> Option<PinState> {
    match ok(client, session, Request::WhoAmI).await {
        Reply::WhoAmI(user) => user.pin,
        other => panic!("not WhoAmI: {other:?}"),
    }
}

/// "Set · 4 digits" in a client's preferences, and a PIN pad's boxes.
#[tokio::test]
async fn who_am_i_says_whether_there_is_a_pin_and_how_many_digits() {
    let server =
        TestServer::start_with(fresh_dir("pin-state").await, ServerOptions::default()).await;
    server.add_user("alice", PASSWORD, None).await;
    let (tv, session) = alice_on(&server).await;
    assert!(pin_state(&tv, &session).await.is_none());
    ok(
        &tv,
        &session,
        Request::SetPin {
            pin: "123456".into(),
            proof: None,
        },
    )
    .await;
    assert_eq!(
        pin_state(&tv, &session).await,
        Some(PinState { digits: Some(6) })
    );
    ok(
        &tv,
        &session,
        Request::ClearPin {
            proof: PinProof::Pin {
                pin: "123456".into(),
            },
        },
    )
    .await;
    assert!(pin_state(&tv, &session).await.is_none());
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_locked_profile_says_how_long_its_pin_is() {
    let (server, tv, session) = locked("pin-state-locked", ServerOptions::default()).await;
    assert!(who(&tv, &session).await);
    assert_eq!(
        pin_state(&tv, &session).await,
        Some(PinState {
            digits: Some(PIN.len() as u8)
        })
    );
    tv.close().await;
    server.stop().await;
}

fn set_pin(pin: &str, proof: Option<PinProof>) -> Request {
    Request::SetPin {
        pin: pin.into(),
        proof,
    }
}

fn clear_pin(proof: PinProof) -> Request {
    Request::ClearPin { proof }
}

fn by_pin(pin: &str) -> PinProof {
    PinProof::Pin { pin: pin.into() }
}

fn by_password(password: &str) -> PinProof {
    PinProof::Password {
        password: password.into(),
    }
}

async fn pin_server(test: &str, options: ServerOptions) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start_with(fresh_dir(test).await, options).await;
    server.add_user("alice", PASSWORD, None).await;
    let (tv, session) = alice_on(&server).await;
    (server, tv, session)
}

/// The first PIN needs no proof. After that, changing or removing it takes
/// the current PIN or the account password (a Change PIN or Remove PIN
/// dialog asks for one); none is `InvalidInput`, a wrong one changes
/// nothing.
#[tokio::test]
async fn a_pin_is_changed_or_removed_only_with_the_current_pin_or_the_password() {
    let (server, tv, session) = pin_server("pin-proof", ServerOptions::default()).await;
    ok(&tv, &session, set_pin(PIN, None)).await;

    let refused = err(&tv, &session, set_pin("98765", None)).await;
    assert!(matches!(refused, Error::InvalidInput { .. }), "{refused:?}");
    for wrong in [
        set_pin("98765", Some(by_pin("0000"))),
        set_pin("98765", Some(by_password("wrong password"))),
        // Neither stands in for the other.
        set_pin("98765", Some(by_pin(PASSWORD))),
        set_pin("98765", Some(by_password(PIN))),
        clear_pin(by_pin("0000")),
        clear_pin(by_password("wrong password")),
    ] {
        assert_eq!(err(&tv, &session, wrong).await, Error::InvalidCredentials);
    }
    // Nothing changed: the PIN is the first one still.
    assert_eq!(
        pin_state(&tv, &session).await,
        Some(PinState { digits: Some(4) })
    );

    ok(&tv, &session, set_pin("98765", Some(by_pin(PIN)))).await;
    assert_eq!(
        pin_state(&tv, &session).await,
        Some(PinState { digits: Some(5) })
    );
    ok(
        &tv,
        &session,
        set_pin("123456", Some(by_password(PASSWORD))),
    )
    .await;
    assert_eq!(
        pin_state(&tv, &session).await,
        Some(PinState { digits: Some(6) })
    );
    ok(&tv, &session, clear_pin(by_pin("123456"))).await;
    assert!(pin_state(&tv, &session).await.is_none());
    ok(&tv, &session, set_pin(PIN, None)).await;
    ok(&tv, &session, clear_pin(by_password(PASSWORD))).await;
    assert!(pin_state(&tv, &session).await.is_none());
    tv.close().await;
    server.stop().await;
}

/// Forgot the PIN: the password alone replaces it, and then removes it.
/// The old PIN no longer unlocks; the new one does.
#[tokio::test]
async fn the_password_replaces_a_forgotten_pin() {
    let (server, tv, session) = pin_server("pin-forgot", ServerOptions::default()).await;
    ok(&tv, &session, set_pin(PIN, None)).await;
    ok(&tv, &session, set_pin("2468", Some(by_password(PASSWORD)))).await;
    assert_eq!(
        err(&tv, &session, Request::UnlockProfile { pin: PIN.into() }).await,
        Error::InvalidCredentials
    );
    ok(&tv, &session, Request::UnlockProfile { pin: "2468".into() }).await;
    ok(&tv, &session, clear_pin(by_password(PASSWORD))).await;
    assert!(pin_state(&tv, &session).await.is_none());
    tv.close().await;
    server.stop().await;
}

/// A locked session gets `ProfileLocked` whatever the proof, and checking
/// it unlocks nothing: the session stays locked.
#[tokio::test]
async fn no_proof_gets_round_the_lock() {
    let (server, tv, session) = locked("pin-proof-locked", ServerOptions::default()).await;
    for request in [
        set_pin("98765", Some(by_pin(PIN))),
        set_pin("98765", Some(by_password(PASSWORD))),
        clear_pin(by_pin(PIN)),
        clear_pin(by_password(PASSWORD)),
    ] {
        assert_eq!(err(&tv, &session, request).await, Error::ProfileLocked);
    }
    assert!(who(&tv, &session).await);
    assert_eq!(
        err(&tv, &session, Request::Libraries).await,
        Error::ProfileLocked
    );
    assert_eq!(
        pin_state(&tv, &session).await,
        Some(PinState { digits: Some(4) })
    );
    tv.close().await;
    server.stop().await;
}

/// Wrong current PINs count towards the same limit as wrong PINs at the
/// lock screen: guessing through Change PIN, Remove PIN and the lock in
/// turn is stopped after as many tries as through the lock alone, and
/// stopped even for the right PIN. The password limit is untouched.
#[tokio::test]
async fn wrong_current_pins_share_the_unlock_limit() {
    let (server, tv, session) = pin_server("pin-proof-limit", ServerOptions::default()).await;
    ok(&tv, &session, set_pin(PIN, None)).await;
    let guesses = [
        set_pin("98765", Some(by_pin("0000"))),
        clear_pin(by_pin("0001")),
        Request::UnlockProfile { pin: "0002".into() },
        set_pin("98765", Some(by_pin("0003"))),
        clear_pin(by_pin("0004")),
    ];
    for guess in guesses {
        assert_eq!(err(&tv, &session, guess).await, Error::InvalidCredentials);
    }
    for right in [
        clear_pin(by_pin(PIN)),
        set_pin("98765", Some(by_pin(PIN))),
        Request::UnlockProfile { pin: PIN.into() },
    ] {
        let refused = err(&tv, &session, right).await;
        assert!(matches!(refused, Error::RateLimited { .. }), "{refused:?}");
    }
    assert_eq!(
        pin_state(&tv, &session).await,
        Some(PinState { digits: Some(4) })
    );
    // Not one password was counted: alice signs in from here.
    tv.login("alice", PASSWORD).await;
    tv.close().await;
    server.stop().await;
}

/// Wrong passwords through Change PIN and Remove PIN are failed sign-ins:
/// with wrong `LoginPassword`s from the same device they reach the one
/// limit, after which even the right password is refused, there and at
/// sign-in, and any sign-in from that device. The PIN's own limit is
/// untouched.
#[tokio::test]
async fn wrong_passwords_share_the_sign_in_limit_not_the_pin_s() {
    let (server, tv, session) = pin_server("pin-password-limit", ServerOptions::default()).await;
    server.add_user("bob", "battery staple", None).await;
    ok(&tv, &session, set_pin(PIN, None)).await;
    let wrong_login = Request::LoginPassword(LoginPassword {
        username: "alice".into(),
        password: "wrong password".into(),
        remember: false,
    });
    assert_eq!(
        tv.call(None, wrong_login.clone()).await,
        Response::Err(Error::InvalidCredentials)
    );
    for guess in [
        set_pin("98765", Some(by_password("wrong 1"))),
        clear_pin(by_password("wrong 2")),
        set_pin("98765", Some(by_password("wrong 3"))),
    ] {
        assert_eq!(err(&tv, &session, guess).await, Error::InvalidCredentials);
    }
    assert_eq!(
        tv.call(None, wrong_login).await,
        Response::Err(Error::InvalidCredentials)
    );
    for right in [
        set_pin("98765", Some(by_password(PASSWORD))),
        clear_pin(by_password(PASSWORD)),
    ] {
        let refused = err(&tv, &session, right).await;
        assert!(matches!(refused, Error::RateLimited { .. }), "{refused:?}");
    }
    for (username, password) in [("alice", PASSWORD), ("bob", "battery staple")] {
        let login = Request::LoginPassword(LoginPassword {
            username: username.into(),
            password: password.into(),
            remember: false,
        });
        let refused = tv.call(None, login).await;
        assert!(
            matches!(refused, Response::Err(Error::RateLimited { .. })),
            "{username}: {refused:?}"
        );
    }
    assert_eq!(
        pin_state(&tv, &session).await,
        Some(PinState { digits: Some(4) })
    );
    // Not one PIN was counted: the PIN proof still works, first try.
    ok(&tv, &session, set_pin("98765", Some(by_pin(PIN)))).await;
    tv.close().await;
    server.stop().await;
}

/// A user who signs in with a passkey (or SSO) only has no password to
/// give: a password proof is answered `NoPassword`, distinctly, and
/// nothing is counted or changed. The PIN proof works for them as for
/// anyone.
#[tokio::test]
async fn a_user_without_a_password_proves_it_with_the_pin() {
    let server =
        TestServer::start_with(fresh_dir("pin-no-password").await, ServerOptions::default()).await;
    let alice = server
        .store
        .add_first_admin_with_passkey(
            "alice",
            FirstPasskey {
                handle: "handle",
                credential_id: "credential",
                name: "Key",
                credential: "{}",
                rp_id: "localhost",
            },
        )
        .await
        .unwrap();
    let tv = server.client().await;
    tv.hello_with(None, Some(mkv_capabilities())).await;
    // As a passkey sign-in or a device approval would open it.
    let token = SessionToken("alice-on-the-tv".into());
    server
        .store
        .create_session(
            &token.hash(),
            alice,
            &SessionBinding::Endpoint {
                endpoint_id: tv.endpoint.id().to_string(),
                device_name: None,
                platform: Platform::Linux,
            },
            i64::MAX / 2,
        )
        .await
        .unwrap();
    let session = SessionId(token.0);
    ok(&tv, &session, set_pin(PIN, None)).await;
    for _ in 0..6 {
        assert_eq!(
            err(&tv, &session, set_pin("98765", Some(by_password("")))).await,
            Error::NoPassword
        );
        assert_eq!(
            err(&tv, &session, clear_pin(by_password(""))).await,
            Error::NoPassword
        );
    }
    assert_eq!(
        pin_state(&tv, &session).await,
        Some(PinState { digits: Some(4) })
    );
    ok(&tv, &session, set_pin("98765", Some(by_pin(PIN)))).await;
    ok(&tv, &session, clear_pin(by_pin("98765"))).await;
    assert!(pin_state(&tv, &session).await.is_none());
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn replaying_an_unlock_does_not_unlock_a_new_connection() {
    use spoolframe_protocol::{RequestEnvelope, rpc::RpcRequest};
    let (server, tv, session) = locked("pin-replay", ServerOptions::default()).await;
    let request = RpcRequest {
        envelope: RequestEnvelope {
            session: Some(session.clone()),
            request: Request::UnlockProfile { pin: PIN.into() },
        },
        operation: Some(tv.operation()),
        expected_revision: None,
    };
    assert_eq!(
        tv.wire(&request).await.reply.response,
        Response::Ok(Reply::Done)
    );
    let other = tv.reconnect(&server).await;
    assert_eq!(
        other.wire(&request).await.reply.response,
        Response::Err(Error::ProfileLocked)
    );
    assert!(who(&other, &session).await);
    other.close().await;
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_old_unlocked_session_cannot_disable_pin_protection() {
    let (server, tv, session) = locked("pin-recent", ServerOptions::default()).await;
    ok(&tv, &session, Request::UnlockProfile { pin: PIN.into() }).await;
    let hash = SessionToken(session.0.clone()).hash();
    crate::app_passkeys::sql(
        &server,
        "UPDATE sessions SET authenticated_at = authenticated_at - 3600 WHERE token_hash = ?1",
        &hash,
    )
    .await;
    assert_eq!(
        err(
            &tv,
            &session,
            Request::SetPinRequired {
                device: None,
                required: false
            }
        )
        .await,
        Error::RecentSignInRequired
    );
    let Reply::Devices { devices } = ok(&tv, &session, Request::ListDevices).await else {
        panic!("devices");
    };
    assert!(devices[0].pin_required);
    server.store.confirm_session(&hash).await.unwrap();
    ok(
        &tv,
        &session,
        Request::SetPinRequired {
            device: None,
            required: false,
        },
    )
    .await;
    tv.close().await;
    server.stop().await;
}

/// A first PIN is set only from a recent sign-in, as the web UI's `pinSet`
/// asks (A-14): an unlocked session left on a TV must not let whoever
/// holds the remote put a PIN on the account. Its owner then would need
/// that PIN, or the password, to change it.
#[tokio::test]
async fn an_old_session_cannot_set_a_first_pin() {
    let (server, tv, session) = pin_server("pin-first-recent", ServerOptions::default()).await;
    let hash = SessionToken(session.0.clone()).hash();
    crate::app_passkeys::sql(
        &server,
        "UPDATE sessions SET authenticated_at = authenticated_at - 3600 WHERE token_hash = ?1",
        &hash,
    )
    .await;
    assert_eq!(
        err(&tv, &session, set_pin(PIN, None)).await,
        Error::RecentSignInRequired
    );
    assert!(pin_state(&tv, &session).await.is_none());
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_fresh_session_sets_a_first_pin() {
    let (server, tv, session) = pin_server("pin-first-fresh", ServerOptions::default()).await;
    ok(&tv, &session, set_pin(PIN, None)).await;
    assert!(pin_state(&tv, &session).await.is_some());
    tv.close().await;
    server.stop().await;
}
