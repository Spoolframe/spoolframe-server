//! Refused requests are logged (finding 46): one line each, at WARN, on
//! the target `spoolframe::refused`, naming what was asked, from where,
//! by whom if known, and why. A flood of them is cut to a few lines a
//! minute and a count. No secret is ever in a line.
//!
//! Before this, a consumer calling admin methods, a session token used
//! from another device and a failed sign-in left no line at all (the
//! PX-11 live probe).

use jsonrpsee::ws_client::WsClient;
use serde_json::json;
use spoolframe_admin_api::{AdminApiClient, MeApiClient, NewUser, Role, error_code};
use spoolframe_server::admin::{self, Origins, SetupCode, TrustedProxies};
use spoolframe_server::auth::Auth;
use spoolframe_server::clock::Clock;
use spoolframe_server::store::Store;
use std::net::SocketAddr;

use spoolframe_protocol::resource::{ResourceRequest, ResourceResponse};
use spoolframe_protocol::{Error, LoginPassword, MediaRead, Reply, Request, Response, SessionId};
use spoolframe_server::auth::SessionToken;

use crate::admin::{CODE, anonymous, code, creds, setup, state_dir, with_token};
use crate::support::{LogCapture, Logged, TestClient, TestServer, alone, mkv_capabilities, movie};

const ROOT_PASSWORD: &str = "correct horse";
const ALICE_PASSWORD: &str = "battery staple";
/// What a failed sign-in tries: it must never reach the log.
const SECRET: &str = "hunter2-never-logged";

/// The one line the refused-request log has, or a failure listing them.
fn one(capture: &LogCapture) -> Logged {
    let lines = capture.refused();
    assert_eq!(lines.len(), 1, "{lines:#?}");
    lines.into_iter().next().unwrap()
}

fn assert_fields(line: &Logged, expected: &[(&str, &str)]) {
    for (name, value) in expected {
        assert_eq!(line.field(name), Some(*value), "{name} in {line:#?}");
    }
}

/// An admin API on a store timed by `clock`, with root as its admin and
/// alice as a consumer: the address, root's client and alice's token.
async fn root_and_alice(
    test: &str,
    clock: Clock,
) -> (SocketAddr, WsClient, String, spoolframe_test_dir::TestDir) {
    let dir = state_dir(test).await;
    let store = Store::open(dir.as_ref()).await.unwrap().with_clock(clock);
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        crate::admin::admin(Auth::new(store), Some(SetupCode::new(CODE))),
        Origins::default(),
        TrustedProxies::default(),
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    let root = anonymous(addr)
        .await
        .setup(setup(CODE, "root", ROOT_PASSWORD))
        .await
        .unwrap();
    let root = with_token(addr, &root.token).await;
    root.user_add(NewUser {
        username: "alice".into(),
        display_name: None,
        password: ALICE_PASSWORD.into(),
        role: Role::Consumer,
    })
    .await
    .unwrap();
    let alice = anonymous(addr)
        .await
        .login(creds("alice", ALICE_PASSWORD))
        .await
        .unwrap();
    (addr, root, alice.token, dir)
}

#[tokio::test]
async fn a_consumer_calling_an_admin_method_is_logged_once() {
    if !alone("refusals::a_consumer_calling_an_admin_method_is_logged_once") {
        return;
    }
    let (addr, _root, alice, _dir) = root_and_alice("refused-forbidden", Clock::default()).await;
    let alice = with_token(addr, &alice).await;
    let (capture, _guard) = LogCapture::start();
    let err = alice.user_list().await.unwrap_err();
    assert_eq!(code(err), error_code::FORBIDDEN);
    let line = one(&capture);
    assert_eq!(line.level, tracing::Level::WARN);
    assert_fields(
        &line,
        &[
            ("kind", "forbidden"),
            ("channel", "admin_api"),
            ("request", "admin_userList"),
            ("source", "127.0.0.1"),
            ("user", "alice"),
            ("reason", "admins only"),
        ],
    );
}

#[tokio::test]
async fn an_old_session_making_a_sensitive_change_is_logged_once() {
    if !alone("refusals::an_old_session_making_a_sensitive_change_is_logged_once") {
        return;
    }
    let (_, root, _, dir) = root_and_alice("refused-recent", Clock::default()).await;
    crate::recent_admin::age(dir.as_ref(), "root").await;
    let (capture, _guard) = LogCapture::start();
    let err = root
        .user_set_password(creds("alice", "a new password"))
        .await
        .unwrap_err();
    assert_eq!(code(err), error_code::RECENT_SIGN_IN_REQUIRED);
    let line = one(&capture);
    assert_eq!(line.level, tracing::Level::WARN);
    assert_fields(
        &line,
        &[
            ("kind", "recent_sign_in_required"),
            ("request", "admin_userSetPassword"),
            ("user", "root"),
        ],
    );
}

/// A token that names no live session is worth a warning: it was stolen,
/// guessed or is stale. No token at all is what every signed-out caller
/// sends, so it is only INFO.
#[tokio::test]
async fn a_made_up_token_is_a_warning_and_no_token_only_info() {
    if !alone("refusals::a_made_up_token_is_a_warning_and_no_token_only_info") {
        return;
    }
    let (addr, _root, _, _dir) = root_and_alice("refused-token", Clock::default()).await;
    let (capture, _guard) = LogCapture::start();
    let err = with_token(addr, "made-up")
        .await
        .user_list()
        .await
        .unwrap_err();
    assert_eq!(code(err), error_code::UNAUTHENTICATED);
    let line = one(&capture);
    assert_eq!(line.level, tracing::Level::WARN);
    assert_fields(
        &line,
        &[
            ("kind", "unauthenticated"),
            ("request", "admin_userList"),
            ("reason", "no such session"),
        ],
    );
    assert!(
        !capture.text().contains("made-up"),
        "the token was logged: {}",
        capture.text()
    );

    capture.clear();
    let err = anonymous(addr).await.user_list().await.unwrap_err();
    assert_eq!(code(err), error_code::UNAUTHENTICATED);
    let line = one(&capture);
    assert_eq!(line.level, tracing::Level::INFO);
    assert_fields(&line, &[("kind", "no_session"), ("reason", "no session")]);
}

#[tokio::test]
async fn a_failed_login_names_the_user_and_why_but_never_the_password() {
    if !alone("refusals::a_failed_login_names_the_user_and_why_but_never_the_password") {
        return;
    }
    let (addr, _root, _, _dir) = root_and_alice("refused-login", Clock::default()).await;
    let (capture, _guard) = LogCapture::start();
    let err = anonymous(addr)
        .await
        .login(creds("mallory", SECRET))
        .await
        .unwrap_err();
    assert_eq!(code(err), error_code::INVALID_CREDENTIALS);
    let line = one(&capture);
    assert_eq!(line.level, tracing::Level::WARN);
    assert_fields(
        &line,
        &[
            ("kind", "sign_in_failed"),
            ("request", "me_login"),
            ("source", "127.0.0.1"),
            ("user", "mallory"),
            ("reason", "unknown user"),
        ],
    );

    capture.clear();
    let err = anonymous(addr)
        .await
        .login(creds("alice", SECRET))
        .await
        .unwrap_err();
    assert_eq!(code(err), error_code::INVALID_CREDENTIALS);
    assert_fields(
        &one(&capture),
        &[("user", "alice"), ("reason", "wrong password")],
    );
    assert!(!capture.text().contains(SECRET), "the password was logged");
}

#[tokio::test]
async fn a_rate_limited_login_is_logged() {
    if !alone("refusals::a_rate_limited_login_is_logged") {
        return;
    }
    let (addr, _root, _, _dir) = root_and_alice("refused-limited", Clock::default()).await;
    let client = anonymous(addr).await;
    for _ in 0..spoolframe_server::auth::MAX_FAILURES {
        client.login(creds("alice", SECRET)).await.unwrap_err();
    }
    let (capture, _guard) = LogCapture::start();
    let err = client.login(creds("alice", SECRET)).await.unwrap_err();
    assert_eq!(code(err), error_code::RATE_LIMITED);
    let line = one(&capture);
    assert_eq!(line.level, tracing::Level::WARN);
    assert_fields(
        &line,
        &[
            ("kind", "rate_limited"),
            ("request", "me_login"),
            ("user", "alice"),
            ("reason", "rate-limited"),
        ],
    );
    assert!(!capture.text().contains(SECRET), "the password was logged");
}

#[tokio::test]
async fn a_wrong_setup_code_is_logged_but_not_the_code() {
    if !alone("refusals::a_wrong_setup_code_is_logged_but_not_the_code") {
        return;
    }
    let (addr, _dir) = crate::admin::server("refused-setup-code").await;
    let (capture, _guard) = LogCapture::start();
    let err = anonymous(addr)
        .await
        .setup(setup("AAAA-BBBB-CCCC", "root", SECRET))
        .await
        .unwrap_err();
    assert_eq!(code(err), error_code::INVALID_SETUP_CODE);
    assert_fields(
        &one(&capture),
        &[
            ("kind", "sign_in_failed"),
            ("request", "admin_setup"),
            ("reason", "wrong setup code"),
        ],
    );
    let text = capture.text();
    assert!(!text.contains("AAAA"), "the code was logged");
    assert!(!text.contains(SECRET), "the password was logged");
}

/// The browser's sign-in is not JSON-RPC: `/auth/login` logs its own.
#[tokio::test]
async fn a_failed_web_sign_in_is_logged() {
    if !alone("refusals::a_failed_web_sign_in_is_logged") {
        return;
    }
    let (addr, _root, _, _dir) = root_and_alice("refused-web-login", Clock::default()).await;
    let (capture, _guard) = LogCapture::start();
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/auth/login"))
        .header("origin", format!("http://{addr}"))
        .json(&json!({ "username": "alice", "password": SECRET }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let line = one(&capture);
    assert_eq!(line.level, tracing::Level::WARN);
    assert_fields(
        &line,
        &[
            ("kind", "sign_in_failed"),
            ("channel", "web"),
            ("request", "/auth/login"),
            ("source", "127.0.0.1"),
            ("user", "alice"),
            ("reason", "wrong password"),
        ],
    );
    assert!(!capture.text().contains(SECRET), "the password was logged");
}

/// A thousand refusals from one place make at most the per-minute cap of
/// lines; a minute on, one line says how many were left out.
#[tokio::test]
async fn a_flood_of_refusals_is_cut_to_a_few_lines_and_a_count() {
    if !alone("refusals::a_flood_of_refusals_is_cut_to_a_few_lines_and_a_count") {
        return;
    }
    const BURST: usize = 1000;
    let clock = Clock::default();
    let (addr, _root, alice, _dir) = root_and_alice("refused-flood", clock.clone()).await;
    let alice = with_token(addr, &alice).await;
    let (capture, _guard) = LogCapture::start();
    for _ in 0..BURST {
        alice.user_list().await.unwrap_err();
    }
    let logged = capture.refused();
    assert_eq!(
        logged.len(),
        spoolframe_server::refusals::PER_WINDOW as usize,
        "{logged:#?}"
    );

    capture.clear();
    clock.advance(spoolframe_server::refusals::WINDOW);
    alice.user_list().await.unwrap_err();
    let logged = capture.refused();
    let summaries: Vec<&Logged> = logged
        .iter()
        .filter(|line| line.field("suppressed").is_some())
        .collect();
    assert_eq!(summaries.len(), 1, "{logged:#?}");
    let suppressed = BURST - spoolframe_server::refusals::PER_WINDOW as usize;
    assert_fields(
        summaries[0],
        &[
            ("kind", "forbidden"),
            ("source", "127.0.0.1"),
            ("suppressed", &suppressed.to_string()),
        ],
    );
    // The summary and the new minute's first refusal.
    assert_eq!(logged.len(), 2, "{logged:#?}");
}

// The client protocol, over QUIC.

/// A QUIC server with alice, the stand-in movie, and alice signed in on a
/// TV that plays it.
async fn quic_alice(test: &str) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start(test).await;
    server.write_media(1000);
    server.add_user("alice", ALICE_PASSWORD, None).await;
    let tv = server.client().await;
    tv.hello_with(None, Some(mkv_capabilities())).await;
    let session = tv.login("alice", ALICE_PASSWORD).await;
    (server, tv, session)
}

fn short(client: &TestClient) -> String {
    client.endpoint.id().fmt_short().to_string()
}

/// G3: a session token from another device's endpoint, as a thief would
/// use it, names whose it is.
#[tokio::test]
async fn a_session_used_from_another_device_is_logged_with_its_owner() {
    if !alone("refusals::a_session_used_from_another_device_is_logged_with_its_owner") {
        return;
    }
    let (server, tv, session) = quic_alice("refused-quic-g3").await;
    let thief = server.client().await;
    thief.hello().await;
    let (capture, _guard) = LogCapture::start();
    assert_eq!(
        thief.call(Some(&session), Request::WhoAmI).await,
        Response::Err(Error::Unauthenticated)
    );
    let line = one(&capture);
    assert_eq!(line.level, tracing::Level::WARN);
    assert_fields(
        &line,
        &[
            ("kind", "unauthenticated"),
            ("channel", "quic"),
            ("request", "who_am_i"),
            ("source", &short(&thief)),
            ("user", "alice"),
            ("reason", "session bound to another device"),
        ],
    );
    assert!(!capture.text().contains(&session.0), "the token was logged");
    thief.close().await;
    tv.close().await;
    server.stop().await;
}

/// No session at all is what a signed-out app sends: INFO, not WARN.
#[tokio::test]
async fn a_request_without_a_session_is_only_info() {
    if !alone("refusals::a_request_without_a_session_is_only_info") {
        return;
    }
    let (server, tv, _) = quic_alice("refused-quic-anonymous").await;
    let (capture, _guard) = LogCapture::start();
    assert_eq!(
        tv.call(None, Request::WhoAmI).await,
        Response::Err(Error::Unauthenticated)
    );
    let line = one(&capture);
    assert_eq!(line.level, tracing::Level::INFO);
    assert_fields(&line, &[("kind", "no_session"), ("source", &short(&tv))]);
    tv.close().await;
    server.stop().await;
}

/// G4: a profile locked on its device until its PIN.
#[tokio::test]
async fn a_locked_profile_s_request_is_logged() {
    if !alone("refusals::a_locked_profile_s_request_is_logged") {
        return;
    }
    let (server, tv, session) = quic_alice("refused-quic-g4").await;
    for request in [
        Request::SetPin {
            pin: "4321".into(),
            proof: None,
        },
        Request::SetPinRequired {
            device: None,
            required: true,
        },
    ] {
        assert_eq!(
            tv.call(Some(&session), request).await,
            Response::Ok(Reply::Done)
        );
    }
    let tv = tv.reconnect(&server).await;
    let (capture, _guard) = LogCapture::start();
    assert_eq!(
        tv.call(Some(&session), Request::Libraries).await,
        Response::Err(Error::ProfileLocked)
    );
    assert_fields(
        &one(&capture),
        &[
            ("kind", "profile_locked"),
            ("request", "libraries"),
            ("source", &short(&tv)),
            ("user", "alice"),
        ],
    );

    // A wrong PIN is a failed sign-in; the PIN is never logged.
    capture.clear();
    assert_eq!(
        tv.call(
            Some(&session),
            Request::UnlockProfile {
                pin: "97531".into()
            }
        )
        .await,
        Response::Err(Error::InvalidCredentials)
    );
    assert_fields(
        &one(&capture),
        &[
            ("kind", "sign_in_failed"),
            ("request", "unlock_profile"),
            ("user", "alice"),
            ("reason", "wrong PIN"),
        ],
    );
    assert!(!capture.text().contains("97531"), "the PIN was logged");
    tv.close().await;
    server.stop().await;
}

/// RS: a change that needs a recent sign-in, from an old session.
#[tokio::test]
async fn an_old_session_s_sensitive_request_is_logged() {
    if !alone("refusals::an_old_session_s_sensitive_request_is_logged") {
        return;
    }
    let (server, tv, session) = quic_alice("refused-quic-rs").await;
    crate::app_passkeys::sql(
        &server,
        "UPDATE sessions SET authenticated_at = authenticated_at - 3600 WHERE token_hash = ?1",
        &SessionToken(session.0.clone()).hash(),
    )
    .await;
    let (capture, _guard) = LogCapture::start();
    assert_eq!(
        tv.call(
            Some(&session),
            Request::SetPin {
                pin: "4321".into(),
                proof: None,
            }
        )
        .await,
        Response::Err(Error::RecentSignInRequired)
    );
    assert_fields(
        &one(&capture),
        &[
            ("kind", "recent_sign_in_required"),
            ("request", "set_pin"),
            ("user", "alice"),
        ],
    );
    assert!(!capture.text().contains("4321"), "the PIN was logged");
    tv.close().await;
    server.stop().await;
}

/// PB: another user's session on the same device, with alice's handle,
/// over RPC and over a media read.
#[tokio::test]
async fn another_session_s_playback_is_logged() {
    if !alone("refusals::another_session_s_playback_is_logged") {
        return;
    }
    let (server, tv, alice) = quic_alice("refused-quic-pb").await;
    let Response::Ok(Reply::PlaybackStarted { handle, .. }) = tv
        .call(
            Some(&alice),
            Request::StartPlayback {
                id: movie().id().clone(),
                selection: None,
                start_ms: None,
                bitmap: Default::default(),
                link_bitrate: None,
            },
        )
        .await
    else {
        panic!("did not start");
    };
    server.add_user("bob", "bob's password", None).await;
    let bob = tv.login("bob", "bob's password").await;
    let (capture, _guard) = LogCapture::start();
    assert_eq!(
        tv.revision(&bob, &handle).await,
        Err(Error::Unauthenticated)
    );
    assert_fields(
        &one(&capture),
        &[
            ("kind", "unauthenticated"),
            ("request", "playback_revision"),
            ("user", "bob"),
            ("reason", "playback is another session's"),
        ],
    );

    capture.clear();
    // On the wire with alice's real revision, as a thief who learnt the
    // handle would read: the test helper would ask for it as bob first.
    let revision = tv.revision(&alice, &handle).await.unwrap();
    let (response, _) = tv
        .resource(&ResourceRequest::Media {
            request: MediaRead {
                session: bob.clone(),
                handle: handle.clone(),
                offset: 0,
                length: 10,
                epoch: 0,
                segment: None,
            },
            revision,
        })
        .await;
    assert!(
        matches!(
            response,
            ResourceResponse::Error {
                error: Error::Unauthenticated
            }
        ),
        "{response:?}"
    );
    assert_fields(
        &one(&capture),
        &[
            ("kind", "unauthenticated"),
            ("request", "media"),
            ("user", "bob"),
            ("reason", "playback is another session's"),
        ],
    );
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_failed_app_sign_in_names_the_user_and_why_but_never_the_password() {
    if !alone("refusals::a_failed_app_sign_in_names_the_user_and_why_but_never_the_password") {
        return;
    }
    let (server, tv, _) = quic_alice("refused-quic-login").await;
    let (capture, _guard) = LogCapture::start();
    for (username, reason) in [("mallory", "unknown user"), ("alice", "wrong password")] {
        capture.clear();
        let login = Request::LoginPassword(LoginPassword {
            username: username.into(),
            password: SECRET.into(),
            remember: false,
        });
        assert_eq!(
            tv.call(None, login).await,
            Response::Err(Error::InvalidCredentials)
        );
        let line = one(&capture);
        assert_eq!(line.level, tracing::Level::WARN);
        assert_fields(
            &line,
            &[
                ("kind", "sign_in_failed"),
                ("channel", "quic"),
                ("request", "login_password"),
                ("source", &short(&tv)),
                ("user", username),
                ("reason", reason),
            ],
        );
        assert!(!capture.text().contains(SECRET), "the password was logged");
    }
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_rate_limited_app_sign_in_is_logged() {
    if !alone("refusals::a_rate_limited_app_sign_in_is_logged") {
        return;
    }
    let (server, tv, _) = quic_alice("refused-quic-limited").await;
    let login = || {
        Request::LoginPassword(LoginPassword {
            username: "alice".into(),
            password: SECRET.into(),
            remember: false,
        })
    };
    for _ in 0..spoolframe_server::auth::MAX_FAILURES {
        tv.call(None, login()).await;
    }
    let (capture, _guard) = LogCapture::start();
    assert!(
        matches!(
            tv.call(None, login()).await,
            Response::Err(Error::RateLimited { .. })
        ),
        "not limited"
    );
    assert_fields(
        &one(&capture),
        &[
            ("kind", "rate_limited"),
            ("request", "login_password"),
            ("user", "alice"),
            ("reason", "rate-limited"),
        ],
    );
    assert!(!capture.text().contains(SECRET), "the password was logged");
    tv.close().await;
    server.stop().await;
}

// Refused Hellos. An app of another protocol revision, or one that sends
// a Hello the server cannot read, used to leave no line at all: only a
// connect and a disconnect, the same as a working app's.

/// A Hello of protocol revision `revision`.
fn hello_of(revision: u32) -> Request {
    Request::Hello(spoolframe_protocol::Hello {
        protocol_version: revision,
        client_name: "spoolframe-ios".into(),
        client_version: "1.2.3".into(),
        platform: spoolframe_types::Platform::Ios,
        device_name: Some("what the app says".into()),
        capabilities: None,
    })
}

/// The refused-Hello lines so far, not the counts of those left out.
fn hellos(capture: &LogCapture) -> Vec<Logged> {
    capture
        .refused()
        .into_iter()
        .filter(|line| line.field("message") == Some("hello refused"))
        .collect()
}

async fn refused_hello(client: &TestClient, revision: u32) {
    let response = client.call(None, hello_of(revision)).await;
    assert!(
        matches!(response, Response::Err(Error::Unsupported { .. })),
        "{response:?}"
    );
}

const SERVER_PROTOCOL: u32 = spoolframe_protocol::PROTOCOL_VERSION;

#[tokio::test]
async fn a_refused_hello_is_logged_once_an_hour_per_device() {
    if !alone("refusals::a_refused_hello_is_logged_once_an_hour_per_device") {
        return;
    }
    let server = TestServer::start("refused-hello").await;
    let (capture, _guard) = LogCapture::start();
    let phone = server.client().await;
    let newer = SERVER_PROTOCOL + 1;
    refused_hello(&phone, newer).await;
    refused_hello(&phone, newer).await;
    let lines = hellos(&capture);
    assert_eq!(lines.len(), 1, "{lines:#?}");
    let line = &lines[0];
    assert_eq!(line.level, tracing::Level::INFO);
    let client_protocol = newer.to_string();
    let server_protocol = SERVER_PROTOCOL.to_string();
    assert_fields(
        line,
        &[
            ("channel", "quic"),
            ("request", "hello"),
            ("source", &short(&phone)),
            ("client_protocol", &client_protocol),
            ("server_protocol", &server_protocol),
            ("client", "spoolframe-ios 1.2.3"),
            ("reason", "unsupported protocol revision"),
        ],
    );
    // Only a device someone signed in from has a name the server keeps:
    // what a refused Hello says is not taken.
    assert_eq!(line.field("device"), None, "{line:#?}");

    // Another device gets its own line.
    let tablet = server.client().await;
    refused_hello(&tablet, newer).await;
    let lines = hellos(&capture);
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert_fields(&lines[1], &[("source", &short(&tablet))]);

    // An hour on, the phone is logged again, after the count of the one
    // left out.
    server
        .clock
        .advance(std::time::Duration::from_secs(60 * 60));
    refused_hello(&phone, newer).await;
    assert_eq!(hellos(&capture).len(), 3);
    let counts: Vec<Logged> = capture
        .refused()
        .into_iter()
        .filter(|line| line.field("suppressed").is_some())
        .collect();
    assert_eq!(counts.len(), 1, "{counts:#?}");
    assert_eq!(counts[0].level, tracing::Level::INFO);
    assert_fields(
        &counts[0],
        &[
            ("kind", "hello_refused"),
            ("source", &short(&phone)),
            ("suppressed", "1"),
        ],
    );
    phone.close().await;
    tablet.close().await;
    server.stop().await;
}

/// A device someone signed in from is named as the server knows it.
#[tokio::test]
async fn a_refused_hello_names_a_known_device() {
    if !alone("refusals::a_refused_hello_names_a_known_device") {
        return;
    }
    let server = TestServer::start("refused-hello-known").await;
    server.add_user("alice", ALICE_PASSWORD, None).await;
    let ipad = server.client().await;
    ipad.hello_as(Some("Kitchen iPad")).await;
    ipad.login("alice", ALICE_PASSWORD).await;
    let (capture, _guard) = LogCapture::start();
    let older = SERVER_PROTOCOL - 1;
    refused_hello(&ipad, older).await;
    let lines = hellos(&capture);
    assert_eq!(lines.len(), 1, "{lines:#?}");
    let client_protocol = older.to_string();
    assert_fields(
        &lines[0],
        &[
            ("device", "Kitchen iPad"),
            ("client_protocol", &client_protocol),
        ],
    );
    ipad.close().await;
    server.stop().await;
}

/// A Hello the server cannot read is refused too, and says what of it
/// could be read.
#[tokio::test]
async fn a_malformed_hello_is_logged() {
    #[derive(serde::Serialize)]
    struct Partial {
        #[serde(rename = "type")]
        kind: &'static str,
        protocol_version: u32,
    }
    #[derive(serde::Serialize)]
    struct Envelope {
        session: Option<String>,
        request: Partial,
    }
    if !alone("refusals::a_malformed_hello_is_logged") {
        return;
    }
    let server = TestServer::start("refused-hello-malformed").await;
    let (capture, _guard) = LogCapture::start();
    let phone = server.client().await;
    let response = phone
        .send(&Envelope {
            session: None,
            request: Partial {
                kind: "hello",
                protocol_version: 99,
            },
        })
        .await;
    assert!(
        matches!(response, Response::Err(Error::Malformed { .. })),
        "{response:?}"
    );
    let lines = hellos(&capture);
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert_fields(&lines[0], &[("client_protocol", "99")]);
    let reason = lines[0].field("reason").unwrap();
    assert!(reason.starts_with("malformed: "), "{reason}");
    phone.close().await;
    server.stop().await;
}
