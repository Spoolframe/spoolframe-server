//! Signing a device in by approving it on another (P3-07): a TV shows a
//! code, a signed-in phone approves it, and the TV gets a session of its
//! own.

use crate::support;

use std::net::SocketAddr;
use std::time::Duration;

use jsonrpsee::ws_client::WsClient;
use spoolframe_admin_api::{MeApiClient, error_code};
use spoolframe_protocol::{
    AuthMethod, DeviceSignInRequest, Error, Reply, Request, Response, SessionId,
};
use support::{ServerOptions, TestClient, TestServer, fresh_dir};

const PASSWORD: &str = "correct horse";

async fn server(test: &str, options: ServerOptions) -> TestServer {
    let server = TestServer::start_with(fresh_dir(test).await, options).await;
    server.add_user("alice", PASSWORD, None).await;
    server
}

/// A TV that said Hello and began a sign-in: its request and code.
async fn tv_begins(server: &TestServer) -> (TestClient, String, String) {
    let tv = server.client().await;
    tv.hello_as(Some("Living room TV")).await;
    match tv.call(None, Request::BeginDeviceSignIn).await {
        Response::Ok(Reply::DeviceSignInStarted {
            request,
            code,
            expires_in_ms,
            web_url,
        }) => {
            assert!(expires_in_ms > 0);
            // Where a browser approves it, if the server's web UI has a
            // public URL.
            let page = server
                .public_url
                .as_ref()
                .map(|origin| format!("{origin}/auth/device?code={code}"));
            assert_eq!(web_url, page);
            (tv, request, code)
        }
        other => panic!("not started: {other:?}"),
    }
}

async fn phone(server: &TestServer) -> (TestClient, SessionId) {
    let phone = server.client().await;
    phone.hello_as(Some("Alice's iPhone")).await;
    let session = phone.login("alice", PASSWORD).await;
    (phone, session)
}

/// The TV waiting on `request`, in the background.
fn tv_awaits(tv: &TestClient, request: &str) -> tokio::task::JoinHandle<Response> {
    let tv = tv.clone();
    let request = request.to_owned();
    tokio::spawn(async move { tv.call(None, Request::AwaitDeviceSignIn { request }).await })
}

fn approve(code: &str) -> Request {
    Request::ApproveDeviceSignIn { code: code.into() }
}

#[tokio::test]
async fn an_approved_tv_gets_a_session_of_its_own() {
    let server = server("dsi-approve", ServerOptions::default()).await;
    let (tv, request, code) = tv_begins(&server).await;
    match tv.call(None, Request::AuthMethods).await {
        Response::Ok(Reply::AuthMethods { methods }) => {
            assert!(methods.contains(&AuthMethod::DeviceApproval));
        }
        other => panic!("{other:?}"),
    }
    let (phone, phone_session) = phone(&server).await;
    // The phone asks whose code it is, typed as people type it.
    let typed = format!("{}-{}", &code[..4], &code[4..]).to_lowercase();
    match phone
        .call(
            Some(&phone_session),
            Request::LookupDeviceSignIn {
                code: typed.clone(),
            },
        )
        .await
    {
        Response::Ok(Reply::DeviceSignInRequest(DeviceSignInRequest {
            device_name,
            client_name,
            ..
        })) => {
            assert_eq!(device_name.as_deref(), Some("Living room TV"));
            assert_eq!(client_name, "test");
        }
        other => panic!("{other:?}"),
    }

    let waiting = tv_awaits(&tv, &request);
    assert_eq!(
        phone.call(Some(&phone_session), approve(&typed)).await,
        Response::Ok(Reply::Done)
    );
    let session = match waiting.await.unwrap() {
        Response::Ok(Reply::LoggedIn { session }) => session,
        other => panic!("not signed in: {other:?}"),
    };
    // Alice's, on the TV.
    match tv.call(Some(&session), Request::WhoAmI).await {
        Response::Ok(Reply::WhoAmI(user)) => assert_eq!(user.username, "alice"),
        other => panic!("{other:?}"),
    }
    // Bound to the TV: useless from the phone.
    assert_eq!(
        phone.call(Some(&session), Request::WhoAmI).await,
        Response::Err(Error::Unauthenticated)
    );
    // One of alice's devices now.
    match phone.call(Some(&phone_session), Request::ListDevices).await {
        Response::Ok(Reply::Devices { devices }) => assert!(
            devices
                .iter()
                .any(|d| d.name.as_deref() == Some("Living room TV")),
            "{devices:?}"
        ),
        other => panic!("{other:?}"),
    }
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_code_is_used_once() {
    let server = server("dsi-once", ServerOptions::default()).await;
    let (tv, request, code) = tv_begins(&server).await;
    let (phone, session) = phone(&server).await;
    assert_eq!(
        phone.call(Some(&session), approve(&code)).await,
        Response::Ok(Reply::Done)
    );
    assert!(matches!(
        phone.call(Some(&session), approve(&code)).await,
        Response::Err(Error::NotFound { .. })
    ));
    // The session is handed over once.
    assert!(matches!(
        tv.call(
            None,
            Request::AwaitDeviceSignIn {
                request: request.clone()
            }
        )
        .await,
        Response::Ok(Reply::LoggedIn { .. })
    ));
    assert_eq!(
        tv.call(None, Request::AwaitDeviceSignIn { request }).await,
        Response::Err(Error::DeviceSignInExpired)
    );
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_denied_sign_in_ends_and_cannot_be_approved_after() {
    let server = server("dsi-deny", ServerOptions::default()).await;
    let (tv, request, code) = tv_begins(&server).await;
    let (phone, session) = phone(&server).await;
    let waiting = tv_awaits(&tv, &request);
    assert_eq!(
        phone
            .call(
                Some(&session),
                Request::DenyDeviceSignIn { code: code.clone() }
            )
            .await,
        Response::Ok(Reply::Done)
    );
    assert_eq!(
        waiting.await.unwrap(),
        Response::Err(Error::DeviceSignInDenied)
    );
    assert!(matches!(
        phone.call(Some(&session), approve(&code)).await,
        Response::Err(Error::NotFound { .. })
    ));
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn only_the_device_that_began_it_gets_the_session() {
    let server = server("dsi-bound", ServerOptions::default()).await;
    let (tv, request, code) = tv_begins(&server).await;
    // Another device with the request id (seen over a shoulder, say).
    let thief = server.client().await;
    thief.hello().await;
    assert_eq!(
        thief
            .call(
                None,
                Request::AwaitDeviceSignIn {
                    request: request.clone()
                }
            )
            .await,
        Response::Err(Error::DeviceSignInExpired)
    );
    let (phone, session) = phone(&server).await;
    phone.call(Some(&session), approve(&code)).await;
    assert_eq!(
        thief
            .call(
                None,
                Request::AwaitDeviceSignIn {
                    request: request.clone()
                }
            )
            .await,
        Response::Err(Error::DeviceSignInExpired)
    );
    assert!(matches!(
        tv.call(None, Request::AwaitDeviceSignIn { request }).await,
        Response::Ok(Reply::LoggedIn { .. })
    ));
    tv.close().await;
    thief.close().await;
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_code_expires() {
    let options = ServerOptions {
        sign_in_lifetime: Duration::from_millis(500),
        ..ServerOptions::default()
    };
    let server = server("dsi-expire", options).await;
    let (tv, request, code) = tv_begins(&server).await;
    // Waiting outlasts the code.
    assert_eq!(
        tv.call(None, Request::AwaitDeviceSignIn { request }).await,
        Response::Err(Error::DeviceSignInExpired)
    );
    let (phone, session) = phone(&server).await;
    assert!(matches!(
        phone.call(Some(&session), approve(&code)).await,
        Response::Err(Error::NotFound { .. })
    ));
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn approving_needs_a_session_on_another_device() {
    let server = server("dsi-who", ServerOptions::default()).await;
    let (tv, _, code) = tv_begins(&server).await;
    // No session: not even a lookup.
    assert_eq!(
        tv.call(None, approve(&code)).await,
        Response::Err(Error::Unauthenticated)
    );
    assert_eq!(
        tv.call(None, Request::LookupDeviceSignIn { code: code.clone() })
            .await,
        Response::Err(Error::Unauthenticated)
    );
    // Signed in on the TV itself: it cannot approve its own code.
    let own = tv.login("alice", PASSWORD).await;
    assert!(matches!(
        tv.call(Some(&own), approve(&code)).await,
        Response::Err(Error::InvalidInput { .. })
    ));
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_locked_phone_cannot_approve() {
    let server = server("dsi-locked", ServerOptions::default()).await;
    let (tv, _, code) = tv_begins(&server).await;
    let (phone, session) = phone(&server).await;
    phone
        .call(Some(&session), Request::SetPin { pin: "4321".into() })
        .await;
    phone
        .call(
            Some(&session),
            Request::SetPinRequired {
                device: None,
                required: true,
            },
        )
        .await;
    let phone = phone.reconnect(&server).await;
    assert_eq!(
        phone.call(Some(&session), approve(&code)).await,
        Response::Err(Error::ProfileLocked)
    );
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn guessing_codes_is_rate_limited() {
    let server = server("dsi-guess", ServerOptions::default()).await;
    let (tv, _, code) = tv_begins(&server).await;
    let (phone, session) = phone(&server).await;
    // None of them can be a code (vowels, digits, lengths); each counts.
    for guess in ["AAAAAAAA", "not a code", "BCDF", "12345678", "EEEE-EEEE"] {
        assert!(matches!(
            phone
                .call(
                    Some(&session),
                    Request::LookupDeviceSignIn { code: guess.into() }
                )
                .await,
            Response::Err(Error::NotFound { .. })
        ));
    }
    // Even the right code now waits.
    assert!(matches!(
        phone.call(Some(&session), approve(&code)).await,
        Response::Err(Error::RateLimited { .. })
    ));
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_device_has_few_codes_pending() {
    let server = server("dsi-many", ServerOptions::default()).await;
    let tv = server.client().await;
    tv.hello().await;
    for _ in 0..spoolframe_server::device_sign_in::MAX_PENDING_PER_DEVICE {
        assert!(matches!(
            tv.call(None, Request::BeginDeviceSignIn).await,
            Response::Ok(Reply::DeviceSignInStarted { .. })
        ));
    }
    assert!(matches!(
        tv.call(None, Request::BeginDeviceSignIn).await,
        Response::Err(Error::RateLimited { .. })
    ));
    tv.close().await;
    server.stop().await;
}

/// Where browsers reach the web UI of the servers below.
const WEB: &str = "https://media.example.org";

/// A server whose web UI has a public URL, and its admin API: what a
/// browser on a phone talks to.
async fn server_with_web(test: &str, options: ServerOptions) -> (TestServer, SocketAddr) {
    let server = server(
        test,
        ServerOptions {
            public_url: Some(WEB.into()),
            ..options
        },
    )
    .await;
    let web = crate::admin::serve_beside(server.auth.clone(), WEB).await;
    (server, web)
}

/// Alice signed in to the web UI with her password.
async fn alice_on_the_web(web: SocketAddr) -> WsClient {
    let login = crate::admin::anonymous(web)
        .await
        .login(crate::admin::creds("alice", PASSWORD))
        .await
        .unwrap();
    crate::admin::with_token(web, &login.token).await
}

/// The third of the user's cases: the app shows a QR code of the web
/// UI's approval page, a phone's camera opens it, its user signs in
/// there and approves. No Spoolframe app on the phone.
#[tokio::test]
async fn a_tv_approved_in_a_browser_gets_a_session_as_the_web_user() {
    let (server, web) = server_with_web("dsi-web", ServerOptions::default()).await;
    let (tv, request, code) = tv_begins(&server).await;
    let alice = alice_on_the_web(web).await;

    // The page asks whose code it is, as the QR code or a person gives it.
    let typed = format!("{}-{}", &code[..4], &code[4..]).to_lowercase();
    let device = alice.device_sign_in_lookup(typed.clone()).await.unwrap();
    assert_eq!(device.device_name.as_deref(), Some("Living room TV"));
    assert_eq!(device.client_name, "test");
    assert!(device.requested_at > 0);

    let waiting = tv_awaits(&tv, &request);
    alice.device_sign_in_approve(typed).await.unwrap();
    let session = match waiting.await.unwrap() {
        Response::Ok(Reply::LoggedIn { session }) => session,
        other => panic!("not signed in: {other:?}"),
    };
    // Alice's, on the TV, and one of her devices from now on.
    match tv.call(Some(&session), Request::WhoAmI).await {
        Response::Ok(Reply::WhoAmI(user)) => assert_eq!(user.username, "alice"),
        other => panic!("{other:?}"),
    }
    let devices = alice.devices().await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name.as_deref(), Some("Living room TV"));

    // The code is spent.
    assert_eq!(
        crate::admin::code(
            alice
                .device_sign_in_approve(code.clone())
                .await
                .unwrap_err()
        ),
        error_code::NO_SUCH_SIGN_IN_CODE
    );
    assert_eq!(
        crate::admin::code(alice.device_sign_in_lookup(code).await.unwrap_err()),
        error_code::NO_SUCH_SIGN_IN_CODE
    );
    tv.close().await;
    server.stop().await;
}

/// The iPhone's part of that case: its browser signs in with the phone's
/// passkey, and that session approves.
#[tokio::test]
async fn a_web_session_made_with_a_passkey_approves_a_tv() {
    use crate::passkeys::{Authenticator, add_passkey, cookie, sign_in_with};
    assert_eq!(WEB, crate::passkeys::ORIGIN);
    let (server, web) = server_with_web("dsi-web-passkey", ServerOptions::default()).await;
    let mut iphone = Authenticator::new();
    add_passkey(&alice_on_the_web(web).await, &mut iphone, "iPhone").await;
    let (tv, request, code) = tv_begins(&server).await;

    let signed_in = sign_in_with(web, &mut iphone).await;
    let session = cookie(&signed_in);
    let token = session.split_once('=').unwrap().1;
    let alice = crate::admin::with_token(web, token).await;
    let waiting = tv_awaits(&tv, &request);
    alice.device_sign_in_approve(code).await.unwrap();
    let session = match waiting.await.unwrap() {
        Response::Ok(Reply::LoggedIn { session }) => session,
        other => panic!("not signed in: {other:?}"),
    };
    match tv.call(Some(&session), Request::WhoAmI).await {
        Response::Ok(Reply::WhoAmI(user)) => assert_eq!(user.username, "alice"),
        other => panic!("{other:?}"),
    }
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_tv_denied_in_a_browser_is_told_so() {
    let (server, web) = server_with_web("dsi-web-deny", ServerOptions::default()).await;
    let (tv, request, code) = tv_begins(&server).await;
    let alice = alice_on_the_web(web).await;
    alice.device_sign_in_deny(code.clone()).await.unwrap();
    assert_eq!(
        tv.call(None, Request::AwaitDeviceSignIn { request }).await,
        Response::Err(Error::DeviceSignInDenied)
    );
    assert_eq!(
        crate::admin::code(alice.device_sign_in_deny(code).await.unwrap_err()),
        error_code::NO_SUCH_SIGN_IN_CODE
    );
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn nobody_approves_in_a_browser_without_signing_in() {
    let (server, web) = server_with_web("dsi-web-anonymous", ServerOptions::default()).await;
    let (tv, _request, code) = tv_begins(&server).await;
    let nobody = crate::admin::anonymous(web).await;
    for refused in [
        nobody.device_sign_in_lookup(code.clone()).await.map(|_| ()),
        nobody.device_sign_in_approve(code.clone()).await,
        nobody.device_sign_in_deny(code.clone()).await,
    ] {
        assert_eq!(
            crate::admin::code(refused.unwrap_err()),
            error_code::UNAUTHENTICATED
        );
    }
    // Still pending: nothing was decided.
    assert!(server.auth.sign_ins().lookup(&code).is_some());
    tv.close().await;
    server.stop().await;
}

/// Codes are short, so guessing them from a browser is limited as it is
/// from a phone, and under the same count for the user.
#[tokio::test]
async fn wrong_codes_from_a_browser_are_limited() {
    let (server, web) = server_with_web("dsi-web-limit", ServerOptions::default()).await;
    let (tv, _request, code) = tv_begins(&server).await;
    let alice = alice_on_the_web(web).await;
    // Three wrong ones from the browser (one of them not a code at all),
    // two from her phone.
    for wrong in ["BBBB-BBBB", "CCCCCCCC", "not a code"] {
        assert_eq!(
            crate::admin::code(alice.device_sign_in_lookup(wrong.into()).await.unwrap_err()),
            error_code::NO_SUCH_SIGN_IN_CODE
        );
    }
    let (phone, session) = phone(&server).await;
    for wrong in ["DDDDDDDD", "FFFFFFFF"] {
        assert!(matches!(
            phone
                .call(
                    Some(&session),
                    Request::LookupDeviceSignIn { code: wrong.into() }
                )
                .await,
            Response::Err(Error::NotFound { .. })
        ));
    }
    // The sixth is not looked at, though the code is right.
    for limited in [
        alice.device_sign_in_lookup(code.clone()).await.map(|_| ()),
        alice.device_sign_in_approve(code.clone()).await,
        alice.device_sign_in_deny(code.clone()).await,
    ] {
        assert_eq!(
            crate::admin::code(limited.unwrap_err()),
            error_code::RATE_LIMITED
        );
    }
    assert!(server.auth.sign_ins().lookup(&code).is_some());
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn an_expired_code_is_not_approved_in_a_browser() {
    let options = ServerOptions {
        sign_in_lifetime: Duration::from_millis(300),
        ..ServerOptions::default()
    };
    let (server, web) = server_with_web("dsi-web-expired", options).await;
    let (tv, _request, code) = tv_begins(&server).await;
    let alice = alice_on_the_web(web).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        crate::admin::code(alice.device_sign_in_approve(code).await.unwrap_err()),
        error_code::NO_SUCH_SIGN_IN_CODE
    );
    tv.close().await;
    server.stop().await;
}
