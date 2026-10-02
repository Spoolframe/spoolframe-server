//! The admin API over a real WebSocket, with the generated client.

use std::net::SocketAddr;
use std::path::Path;

use jsonrpsee::core::ClientError;
use jsonrpsee::ws_client::HeaderMap;
use jsonrpsee::ws_client::{WsClient, WsClientBuilder};
use spoolframe_admin_api::{AdminApiClient, Credentials, NewUser, SetupRequest, error_code};
use spoolframe_server::admin::{self, SetupCode};
use spoolframe_server::auth::Auth;
use spoolframe_server::store::Store;

/// The setup code the test servers accept.
const CODE: &str = "K7QD-M2XP-9RTB";

fn status() -> std::sync::Arc<admin::StatusSource> {
    std::sync::Arc::new(admin::StatusSource::new(
        "Den",
        crate::support::transcoding(),
    ))
}

async fn server(test: &str) -> SocketAddr {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("admin")
        .join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let auth = Auth::new(Store::open(&dir).await.unwrap());
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        auth,
        Some(SetupCode::new(CODE)),
        status(),
        None,
    )
    .await
    .unwrap();
    // Runs for the life of the test process.
    std::mem::forget(handle);
    addr
}

async fn anonymous(addr: SocketAddr) -> WsClient {
    WsClientBuilder::default()
        .build(format!("ws://{addr}"))
        .await
        .unwrap()
}

async fn with_token(addr: SocketAddr, token: &str) -> WsClient {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    WsClientBuilder::default()
        .set_headers(headers)
        .build(format!("ws://{addr}"))
        .await
        .unwrap()
}

fn setup(code: &str, username: &str, password: &str) -> SetupRequest {
    SetupRequest {
        code: code.into(),
        username: username.into(),
        password: password.into(),
    }
}

fn creds(username: &str, password: &str) -> Credentials {
    Credentials {
        username: username.into(),
        password: password.into(),
    }
}

fn code(err: ClientError) -> i32 {
    match err {
        ClientError::Call(e) => e.code(),
        other => panic!("not a call error: {other}"),
    }
}

/// A server with an admin, and a client logged in as them.
async fn admin_client(test: &str) -> (SocketAddr, WsClient) {
    let addr = server(test).await;
    let setup = anonymous(addr)
        .await
        .setup(setup(CODE, "root", "correct horse"))
        .await
        .unwrap();
    (addr, with_token(addr, &setup.token).await)
}

#[tokio::test]
async fn setup_creates_the_first_admin_once() {
    let addr = server("setup").await;
    let client = anonymous(addr).await;
    let first = client
        .setup(setup("k7qd m2xp 9rtb", "Root", "correct horse"))
        .await
        .unwrap();
    assert_eq!(first.username, "root");
    assert!(first.is_admin);
    let again = client
        .setup(setup(CODE, "other", "correct horse"))
        .await
        .unwrap_err();
    assert_eq!(code(again), error_code::ALREADY_SET_UP);
}

#[tokio::test]
async fn an_admin_manages_users() {
    let (addr, admin) = admin_client("manage").await;
    let alice = admin
        .user_add(NewUser {
            username: "alice".into(),
            display_name: None,
            password: "battery staple".into(),
            admin: false,
        })
        .await
        .unwrap();
    assert!(!alice.is_admin);
    let names: Vec<_> = admin
        .user_list()
        .await
        .unwrap()
        .into_iter()
        .map(|u| u.username)
        .collect();
    assert_eq!(names, ["alice", "root"]);

    admin
        .user_set_password(creds("alice", "new password"))
        .await
        .unwrap();
    let login = anonymous(addr)
        .await
        .login(creds("alice", "new password"))
        .await
        .unwrap();
    assert!(!login.is_admin);
}

#[tokio::test]
async fn an_admin_sees_the_server_s_status_and_a_user_does_not() {
    let (addr, admin) = admin_client("status").await;
    let status = admin.server_status().await.unwrap();
    assert_eq!(status.name, "Den");
    assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
    // Nothing detected in tests: the software baseline.
    assert_eq!(status.transcoding.video_encoders, ["libx264"]);
    assert_eq!(status.transcoding.audio_encoders, ["aac"]);

    admin
        .user_add(NewUser {
            username: "alice".into(),
            display_name: None,
            password: "battery staple".into(),
            admin: false,
        })
        .await
        .unwrap();
    let alice = anonymous(addr)
        .await
        .login(creds("alice", "battery staple"))
        .await
        .unwrap();
    let err = with_token(addr, &alice.token)
        .await
        .server_status()
        .await
        .unwrap_err();
    assert_eq!(code(err), error_code::FORBIDDEN);
}

#[tokio::test]
async fn plex_watch_sync_says_when_it_is_not_configured() {
    let (_, admin) = admin_client("no-plex-sync").await;
    let err = admin.plex_watch_sync(true).await.unwrap_err();
    assert_eq!(code(err), error_code::NOT_CONFIGURED);
}

#[tokio::test]
async fn admin_methods_need_a_session() {
    let addr = server("no-session").await;
    let err = anonymous(addr).await.user_list().await.unwrap_err();
    assert_eq!(code(err), error_code::UNAUTHENTICATED);
    let err = with_token(addr, "made-up")
        .await
        .user_list()
        .await
        .unwrap_err();
    assert_eq!(code(err), error_code::UNAUTHENTICATED);
}

#[tokio::test]
async fn a_user_who_is_not_an_admin_is_forbidden() {
    let (addr, admin) = admin_client("forbidden").await;
    admin
        .user_add(NewUser {
            username: "alice".into(),
            display_name: None,
            password: "battery staple".into(),
            admin: false,
        })
        .await
        .unwrap();
    let alice = anonymous(addr)
        .await
        .login(creds("alice", "battery staple"))
        .await
        .unwrap();
    let err = with_token(addr, &alice.token)
        .await
        .user_list()
        .await
        .unwrap_err();
    assert_eq!(code(err), error_code::FORBIDDEN);
}

#[tokio::test]
async fn disabling_a_user_ends_their_session_at_once() {
    let (addr, admin) = admin_client("disable").await;
    admin
        .user_add(NewUser {
            username: "bob".into(),
            display_name: None,
            password: "battery staple".into(),
            admin: true,
        })
        .await
        .unwrap();
    let bob = anonymous(addr)
        .await
        .login(creds("bob", "battery staple"))
        .await
        .unwrap();
    let bob = with_token(addr, &bob.token).await;
    bob.user_list().await.unwrap();

    assert_eq!(admin.user_disable("bob".into()).await.unwrap(), 1);
    let err = bob.user_list().await.unwrap_err();
    assert_eq!(code(err), error_code::UNAUTHENTICATED);
    let err = anonymous(addr)
        .await
        .login(creds("bob", "battery staple"))
        .await
        .unwrap_err();
    assert_eq!(code(err), error_code::INVALID_CREDENTIALS);
}

#[tokio::test]
async fn logout_ends_the_session() {
    let (_, admin) = admin_client("logout").await;
    admin.logout().await.unwrap();
    let err = admin.user_list().await.unwrap_err();
    assert_eq!(code(err), error_code::UNAUTHENTICATED);
}

#[tokio::test]
async fn bad_input_is_named() {
    let (_, admin) = admin_client("bad-input").await;
    let short = admin
        .user_add(NewUser {
            username: "alice".into(),
            display_name: None,
            password: "short".into(),
            admin: false,
        })
        .await
        .unwrap_err();
    assert_eq!(code(short), error_code::INVALID_INPUT);
    let missing = admin.user_disable("nobody".into()).await.unwrap_err();
    assert_eq!(code(missing), error_code::NO_SUCH_USER);
    let dup = admin
        .user_add(NewUser {
            username: "ROOT".into(),
            display_name: None,
            password: "battery staple".into(),
            admin: false,
        })
        .await
        .unwrap_err();
    assert_eq!(code(dup), error_code::USER_EXISTS);
}

#[tokio::test]
async fn setup_needs_the_code_from_the_log() {
    let addr = server("setup-code").await;
    let client = anonymous(addr).await;
    let err = client
        .setup(setup("AAAA-BBBB-CCCC", "root", "correct horse"))
        .await
        .unwrap_err();
    assert_eq!(code(err), error_code::INVALID_SETUP_CODE);
    // Nothing was created: the right code still works.
    client
        .setup(setup(CODE, "root", "correct horse"))
        .await
        .unwrap();
}

#[tokio::test]
async fn guessing_the_setup_code_is_rate_limited() {
    let addr = server("setup-guess").await;
    let client = anonymous(addr).await;
    for _ in 0..spoolframe_server::auth::MAX_FAILURES {
        let _ = client
            .setup(setup("WRNG-WRNG-WRNG", "root", "correct horse"))
            .await;
    }
    let err = client
        .setup(setup(CODE, "root", "correct horse"))
        .await
        .unwrap_err();
    assert_eq!(code(err), error_code::RATE_LIMITED);
}

#[tokio::test]
async fn a_server_started_with_users_offers_no_setup() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("admin")
        .join("no-setup");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let auth = Auth::new(Store::open(&dir).await.unwrap());
    let (addr, handle) = admin::serve("127.0.0.1:0".parse().unwrap(), auth, None, status(), None)
        .await
        .unwrap();
    std::mem::forget(handle);
    let err = anonymous(addr)
        .await
        .setup(setup(CODE, "root", "correct horse"))
        .await
        .unwrap_err();
    assert_eq!(code(err), error_code::ALREADY_SET_UP);
}
