//! Confirming it is you in an app (P3-07): a session signed in long ago
//! proves its user again, with the account password or an app passkey,
//! before a change that needs a recent sign-in, as the web UI's
//! `me.confirmPassword` and `me.confirmPasskey` do. A PIN is not a proof.

use spoolframe_protocol::{
    ConfirmPassword, Error, FinishPasskeySignIn, PasskeyRequest, Reply, Request, Response,
    SessionId,
};
use spoolframe_types::Platform;

use crate::app_passkeys::{App, sql};
use crate::support::{ServerOptions, TestClient, TestServer, fresh_dir};

const PASSWORD: &str = "correct horse";

const CONFIRMED: Response = Response::Ok(Reply::Confirmed);
const REFUSED: Response = Response::Err(Error::InvalidCredentials);
const NOT_RECENT: Response = Response::Err(Error::RecentSignInRequired);

async fn server(test: &str) -> TestServer {
    let server = TestServer::start_with(fresh_dir(test).await, ServerOptions::default()).await;
    server.add_user("alice", PASSWORD, Some("Alice")).await;
    server
}

/// `username` signed in with their password on a phone called `name`.
async fn signed_in(server: &TestServer, username: &str, name: &str) -> (TestClient, SessionId) {
    let phone = server.client().await;
    phone.hello_as(Some(name)).await;
    let session = phone.login(username, PASSWORD).await;
    (phone, session)
}

/// Make `session`'s sign-in an hour older: well past `RECENT_SIGN_IN`.
async fn age(server: &TestServer, session: &SessionId) {
    let hash = spoolframe_server::auth::SessionToken(session.0.clone()).hash();
    sql(
        server,
        "UPDATE sessions SET authenticated_at = authenticated_at - 3600 WHERE token_hash = ?1",
        &hash,
    )
    .await;
}

/// A TV that began a device sign-in: it, and its code.
async fn tv_code(server: &TestServer) -> (TestClient, String) {
    let tv = server.client().await;
    tv.hello_from(Platform::Tvos, Some("Living room TV"), None)
        .await;
    match tv
        .call(None, Request::BeginDeviceSignIn { remember: false })
        .await
    {
        Response::Ok(Reply::DeviceSignInStarted { code, .. }) => (tv, code),
        other => panic!("not started: {other:?}"),
    }
}

fn approve(code: &str) -> Request {
    Request::ApproveDeviceSignIn {
        code: code.into(),
        remember: false,
    }
}

fn confirm_password(password: &str) -> Request {
    Request::ConfirmPassword(ConfirmPassword {
        password: password.into(),
    })
}

/// Make an app passkey on `app` in `session`, which must be recent.
async fn register(device: &TestClient, session: &SessionId, app: &mut App, name: &str) {
    let creation = match device
        .call(Some(session), Request::BeginPasskeyRegistration)
        .await
    {
        Response::Ok(Reply::PasskeyRegistrationBegun(begun)) => {
            begun.for_server(device.conn.remote_id().as_bytes())
        }
        other => panic!("not begun: {other:?}"),
    };
    assert_eq!(
        device
            .call(
                Some(session),
                Request::FinishPasskeyRegistration(app.create(&creation, name))
            )
            .await,
        Response::Ok(Reply::Done)
    );
}

async fn begin_confirm(device: &TestClient, session: &SessionId) -> PasskeyRequest {
    match device
        .call(Some(session), Request::BeginConfirmPasskey)
        .await
    {
        Response::Ok(Reply::PasskeySignInBegun(begun)) => {
            begun.for_server(device.conn.remote_id().as_bytes())
        }
        other => panic!("not begun: {other:?}"),
    }
}

async fn finish_confirm(
    device: &TestClient,
    session: &SessionId,
    signed: FinishPasskeySignIn,
) -> Response {
    device
        .call(Some(session), Request::FinishConfirmPasskey(signed))
        .await
}

/// Alice on her phone with an app passkey made there, her session then
/// aged past a recent sign-in.
async fn alice_old_with_a_passkey(server: &TestServer) -> (TestClient, SessionId, App) {
    let (phone, session) = signed_in(server, "alice", "Alice's iPhone").await;
    let mut app = App::new();
    register(&phone, &session, &mut app, "Alice's iPhone").await;
    age(server, &session).await;
    (phone, session, app)
}

/// The reason for the step: an app signed in days ago approves a TV once
/// its user confirms their password, and only that session is refreshed.
#[tokio::test]
async fn an_old_session_approves_a_tv_after_its_password_is_confirmed() {
    let server = server("confirm-password").await;
    let (tv, code) = tv_code(&server).await;
    let (phone, session) = signed_in(&server, "alice", "Alice's iPhone").await;
    let (tablet, other) = signed_in(&server, "alice", "Alice's iPad").await;
    age(&server, &session).await;
    age(&server, &other).await;
    assert_eq!(phone.call(Some(&session), approve(&code)).await, NOT_RECENT);

    assert_eq!(
        phone.call(Some(&session), confirm_password(PASSWORD)).await,
        CONFIRMED
    );
    assert_eq!(
        phone.call(Some(&session), approve(&code)).await,
        Response::Ok(Reply::Done)
    );
    // Her other device's session proved nothing.
    let (_tv2, code2) = tv_code(&server).await;
    assert_eq!(tablet.call(Some(&other), approve(&code2)).await, NOT_RECENT);
    tv.close().await;
    phone.close().await;
    tablet.close().await;
    server.stop().await;
}

/// The same with an app passkey of hers, asked for by its id alone.
#[tokio::test]
async fn an_old_session_approves_a_tv_after_its_passkey_is_confirmed() {
    let server = server("confirm-passkey").await;
    let (tv, code) = tv_code(&server).await;
    let (phone, session, mut app) = alice_old_with_a_passkey(&server).await;
    assert_eq!(phone.call(Some(&session), approve(&code)).await, NOT_RECENT);

    let request = begin_confirm(&phone, &session).await;
    let signed = app.get(&request);
    assert_eq!(
        request.allow_credential_ids,
        vec![signed.credential_id.clone()]
    );
    assert_eq!(finish_confirm(&phone, &session, signed).await, CONFIRMED);
    assert_eq!(
        phone.call(Some(&session), approve(&code)).await,
        Response::Ok(Reply::Done)
    );
    // Used once.
    assert_eq!(
        finish_confirm(&phone, &session, app.get(&request)).await,
        REFUSED
    );
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

/// A wrong password is a failed sign-in under her username and this
/// device, the keys a login and the web UI's confirmation count: after
/// five, even the right one is refused unchecked, and so is a login.
#[tokio::test]
async fn a_wrong_password_confirms_nothing_and_is_limited() {
    let server = server("confirm-password-limit").await;
    let (tv, code) = tv_code(&server).await;
    let (phone, session) = signed_in(&server, "alice", "Alice's iPhone").await;
    age(&server, &session).await;
    for _ in 0..spoolframe_server::auth::MAX_FAILURES {
        assert_eq!(
            phone
                .call(Some(&session), confirm_password("battery staple"))
                .await,
            REFUSED
        );
    }
    assert_eq!(phone.call(Some(&session), approve(&code)).await, NOT_RECENT);
    assert!(matches!(
        phone.call(Some(&session), confirm_password(PASSWORD)).await,
        Response::Err(Error::RateLimited { .. })
    ));
    // The username's limit is the one a login meets, from anywhere.
    let laptop = server.client().await;
    laptop.hello_as(Some("Alice's laptop")).await;
    assert!(matches!(
        laptop
            .call(
                None,
                Request::LoginPassword(spoolframe_protocol::LoginPassword {
                    username: "alice".into(),
                    password: PASSWORD.into(),
                    remember: false,
                })
            )
            .await,
        Response::Err(Error::RateLimited { .. })
    ));
    tv.close().await;
    phone.close().await;
    laptop.close().await;
    server.stop().await;
}

/// Failed passkey confirmations count as failed passkey sign-ins do.
#[tokio::test]
async fn failed_passkey_confirmations_are_limited() {
    let server = server("confirm-passkey-limit").await;
    let (phone, session, mut app) = alice_old_with_a_passkey(&server).await;
    for _ in 0..spoolframe_server::auth::MAX_FAILURES {
        let request = begin_confirm(&phone, &session).await;
        let mut signed = app.get(&request);
        signed.signature[10] ^= 0xFF;
        assert_eq!(finish_confirm(&phone, &session, signed).await, REFUSED);
    }
    let attempt = phone
        .call(Some(&session), Request::BeginConfirmPasskey)
        .await;
    assert!(
        matches!(attempt, Response::Err(Error::RateLimited { .. })),
        "{attempt:?}"
    );
    phone.close().await;
    server.stop().await;
}

/// Bob's passkey, signing alice's challenge, confirms nothing.
#[tokio::test]
async fn another_user_s_passkey_confirms_nothing() {
    let server = server("confirm-passkey-other").await;
    server.add_user("bob", PASSWORD, None).await;
    let (bob_phone, bob) = signed_in(&server, "bob", "Bob's phone").await;
    let mut bob_app = App::new();
    register(&bob_phone, &bob, &mut bob_app, "Bob's phone").await;
    let (tv, code) = tv_code(&server).await;
    let (phone, session, _app) = alice_old_with_a_passkey(&server).await;

    let mut request = begin_confirm(&phone, &session).await;
    // His platform ignores the list it was given.
    request.allow_credential_ids.clear();
    assert_eq!(
        finish_confirm(&phone, &session, bob_app.get(&request)).await,
        REFUSED
    );
    assert_eq!(phone.call(Some(&session), approve(&code)).await, NOT_RECENT);
    tv.close().await;
    phone.close().await;
    bob_phone.close().await;
    server.stop().await;
}

/// A platform that returns no user handle sends none: alice's passkey
/// still confirms her, and bob's still confirms nothing.
#[tokio::test]
async fn a_passkey_signed_with_no_user_handle_confirms_only_its_user() {
    let server = server("confirm-passkey-no-handle").await;
    server.add_user("bob", PASSWORD, None).await;
    let (bob_phone, bob) = signed_in(&server, "bob", "Bob's phone").await;
    let mut bob_app = App::new();
    register(&bob_phone, &bob, &mut bob_app, "Bob's phone").await;
    let (tv, code) = tv_code(&server).await;
    let (phone, session, mut app) = alice_old_with_a_passkey(&server).await;

    let mut request = begin_confirm(&phone, &session).await;
    request.allow_credential_ids.clear();
    let mut signed = bob_app.get(&request);
    signed.user_handle = None;
    assert_eq!(finish_confirm(&phone, &session, signed).await, REFUSED);
    assert_eq!(phone.call(Some(&session), approve(&code)).await, NOT_RECENT);

    let request = begin_confirm(&phone, &session).await;
    let mut signed = app.get(&request);
    signed.user_handle = None;
    assert_eq!(finish_confirm(&phone, &session, signed).await, CONFIRMED);
    assert_eq!(
        phone.call(Some(&session), approve(&code)).await,
        Response::Ok(Reply::Done)
    );
    tv.close().await;
    phone.close().await;
    bob_phone.close().await;
    server.stop().await;
}

/// An empty user handle confirms nothing: it is malformed, `InvalidInput`,
/// and uses up nothing, so the same signature with no handle confirms.
#[tokio::test]
async fn an_empty_user_handle_confirms_nothing() {
    let server = server("confirm-passkey-empty-handle").await;
    let (tv, code) = tv_code(&server).await;
    let (phone, session, mut app) = alice_old_with_a_passkey(&server).await;
    let request = begin_confirm(&phone, &session).await;
    let mut signed = app.get(&request);
    signed.user_handle = Some(Vec::new());
    assert!(matches!(
        finish_confirm(&phone, &session, signed.clone()).await,
        Response::Err(Error::InvalidInput { .. })
    ));
    assert_eq!(phone.call(Some(&session), approve(&code)).await, NOT_RECENT);
    signed.user_handle = None;
    assert_eq!(finish_confirm(&phone, &session, signed).await, CONFIRMED);
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

/// A confirmation signed for another server, with this one's nonce,
/// confirms nothing (PX-09, finding 2): the challenge names the server.
#[tokio::test]
async fn a_confirmation_signed_for_another_server_confirms_nothing() {
    let server = server("confirm-passkey-other-server").await;
    let (tv, code) = tv_code(&server).await;
    let (phone, session, mut app) = alice_old_with_a_passkey(&server).await;
    let begun = match phone
        .call(Some(&session), Request::BeginConfirmPasskey)
        .await
    {
        Response::Ok(Reply::PasskeySignInBegun(begun)) => begun,
        other => panic!("not begun: {other:?}"),
    };
    let signed = app.get(&begun.for_server(&[0x55; 32]));
    assert_eq!(finish_confirm(&phone, &session, signed).await, REFUSED);
    assert_eq!(phone.call(Some(&session), approve(&code)).await, NOT_RECENT);
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

/// A confirmation's challenge is its session's: a sign-in's, or another
/// session's, does not finish it.
#[tokio::test]
async fn a_confirmation_takes_only_its_own_session_s_challenge() {
    let server = server("confirm-passkey-bound").await;
    let (phone, session, mut app) = alice_old_with_a_passkey(&server).await;

    // A sign-in begun on this device.
    let sign_in = match phone
        .call(None, Request::BeginPasskeySignIn { username: None })
        .await
    {
        Response::Ok(Reply::PasskeySignInBegun(begun)) => {
            begun.for_server(phone.conn.remote_id().as_bytes())
        }
        other => panic!("not begun: {other:?}"),
    };
    assert_eq!(
        finish_confirm(&phone, &session, app.get(&sign_in)).await,
        REFUSED
    );

    // Another session of hers on the same phone.
    let second = phone.login("alice", PASSWORD).await;
    age(&server, &second).await;
    let request = begin_confirm(&phone, &session).await;
    assert_eq!(
        finish_confirm(&phone, &second, app.get(&request)).await,
        REFUSED
    );
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn confirming_needs_a_session() {
    let server = server("confirm-anonymous").await;
    let (phone, _session, mut app) = alice_old_with_a_passkey(&server).await;
    let sign_in = match phone
        .call(None, Request::BeginPasskeySignIn { username: None })
        .await
    {
        Response::Ok(Reply::PasskeySignInBegun(begun)) => {
            begun.for_server(phone.conn.remote_id().as_bytes())
        }
        other => panic!("not begun: {other:?}"),
    };
    for request in [
        confirm_password(PASSWORD),
        Request::BeginConfirmPasskey,
        Request::FinishConfirmPasskey(app.get(&sign_in)),
    ] {
        assert_eq!(
            phone.call(None, request).await,
            Response::Err(Error::Unauthenticated)
        );
    }
    phone.close().await;
    server.stop().await;
}

/// No password, or no app passkey: that way is not offered, and nothing
/// is counted.
#[tokio::test]
async fn a_way_the_user_does_not_have_is_said_so() {
    let server = server("confirm-missing").await;
    let (phone, session) = signed_in(&server, "alice", "Alice's iPhone").await;
    age(&server, &session).await;
    assert_eq!(
        phone
            .call(Some(&session), Request::BeginConfirmPasskey)
            .await,
        Response::Err(Error::NotFound {
            what: "app passkey".into()
        })
    );
    sql(
        &server,
        "DELETE FROM password_credentials WHERE user_id = \
         (SELECT id FROM users WHERE username = ?1)",
        "alice",
    )
    .await;
    assert_eq!(
        phone.call(Some(&session), confirm_password(PASSWORD)).await,
        Response::Err(Error::NoPassword)
    );
    phone.close().await;
    server.stop().await;
}

/// A PIN-locked profile cannot confirm, a PIN is not a confirmation, and
/// a confirmation unlocks nothing.
#[tokio::test]
async fn a_locked_profile_neither_confirms_nor_is_unlocked_by_it() {
    let server = server("confirm-locked").await;
    let (tv, code) = tv_code(&server).await;
    let (phone, session) = signed_in(&server, "alice", "Alice's iPhone").await;
    for request in [
        Request::SetPin {
            pin: "2468".into(),
            proof: None,
        },
        Request::SetPinRequired {
            device: None,
            required: true,
        },
    ] {
        assert_eq!(
            phone.call(Some(&session), request).await,
            Response::Ok(Reply::Done)
        );
    }
    age(&server, &session).await;
    let phone = phone.reconnect(&server).await;
    for request in [
        confirm_password(PASSWORD),
        Request::BeginConfirmPasskey,
        approve(&code),
    ] {
        assert_eq!(
            phone.call(Some(&session), request).await,
            Response::Err(Error::ProfileLocked)
        );
    }
    // Unlocked with the PIN, she has still not confirmed it is her.
    assert_eq!(
        phone
            .call(
                Some(&session),
                Request::UnlockProfile { pin: "2468".into() }
            )
            .await,
        Response::Ok(Reply::Done)
    );
    assert_eq!(phone.call(Some(&session), approve(&code)).await, NOT_RECENT);
    assert_eq!(
        phone.call(Some(&session), confirm_password(PASSWORD)).await,
        CONFIRMED
    );
    // The confirmation is the session's; the lock is the connection's.
    let phone = phone.reconnect(&server).await;
    assert_eq!(
        phone.call(Some(&session), approve(&code)).await,
        Response::Err(Error::ProfileLocked)
    );
    tv.close().await;
    phone.close().await;
    server.stop().await;
}
