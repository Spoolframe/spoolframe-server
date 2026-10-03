//! Signing a device in by approving it on another (P3-07): a TV shows a
//! code, a signed-in phone approves it, and the TV gets a session of its
//! own.

use crate::support;

use std::time::Duration;

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
        }) => {
            assert!(expires_in_ms > 0);
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
