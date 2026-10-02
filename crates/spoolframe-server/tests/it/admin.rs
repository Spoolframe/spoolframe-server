//! The admin API over a real WebSocket, with the generated client.

use std::net::SocketAddr;

use jsonrpsee::core::ClientError;
use jsonrpsee::ws_client::HeaderMap;
use jsonrpsee::ws_client::{WsClient, WsClientBuilder};
use spoolframe_admin_api::{
    AdminApiClient, Credentials, MeApiClient, NewUser, PasswordChange, Role, SetupRequest,
    error_code,
};
use spoolframe_server::admin::{self, SetupCode};
use spoolframe_server::auth::Auth;
use spoolframe_server::store::Store;

/// The setup code the test servers accept.
const CODE: &str = "K7QD-M2XP-9RTB";

/// The status of a server whose key is the first fingerprint test vector.
fn status() -> std::sync::Arc<admin::StatusSource> {
    let status = admin::StatusSource::new("Den", crate::support::transcoding());
    status.set_endpoint_id(VECTOR.0.to_owned());
    std::sync::Arc::new(status)
}

const VECTOR: (&str, &str) = spoolframe_protocol::fingerprint::TEST_VECTORS[0];

/// A directory of the test's own under a random name, removed when
/// dropped, so runs at once never share one.
async fn state_dir(test: &str) -> spoolframe_test_dir::TestDir {
    spoolframe_test_dir::TestDir::new(&format!("spoolframe-admin-{test}")).await
}

/// An admin API on a store in a directory kept for the test.
async fn server(test: &str) -> (SocketAddr, spoolframe_test_dir::TestDir) {
    let dir = state_dir(test).await;
    let auth = Auth::new(Store::open(dir.as_ref()).await.unwrap());
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
    (addr, dir)
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

/// A server with an admin, a client logged in as them, and the server's
/// directory, kept for the test.
async fn admin_client(test: &str) -> (SocketAddr, WsClient, spoolframe_test_dir::TestDir) {
    let (addr, dir) = server(test).await;
    let setup = anonymous(addr)
        .await
        .setup(setup(CODE, "root", "correct horse"))
        .await
        .unwrap();
    (addr, with_token(addr, &setup.token).await, dir)
}

#[tokio::test]
async fn setup_creates_the_first_admin_once() {
    let (addr, _dir) = server("setup").await;
    let client = anonymous(addr).await;
    let first = client
        .setup(setup("k7qd m2xp 9rtb", "Root", "correct horse"))
        .await
        .unwrap();
    assert_eq!(first.username, "root");
    assert_eq!(first.role, Role::Admin);
    let again = client
        .setup(setup(CODE, "other", "correct horse"))
        .await
        .unwrap_err();
    assert_eq!(code(again), error_code::ALREADY_SET_UP);
}

#[tokio::test]
async fn an_admin_manages_users() {
    let (addr, admin, _dir) = admin_client("manage").await;
    let alice = admin
        .user_add(NewUser {
            username: "alice".into(),
            display_name: None,
            password: "battery staple".into(),
            role: Role::Consumer,
        })
        .await
        .unwrap();
    assert_eq!(alice.role, Role::Consumer);
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
    assert_eq!(login.role, Role::Consumer);
}

#[tokio::test]
async fn an_admin_sees_the_server_s_status_and_a_user_does_not() {
    let (addr, admin, _dir) = admin_client("status").await;
    let status = admin.server_status().await.unwrap();
    assert_eq!(status.name, "Den");
    assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(status.endpoint_id.as_deref(), Some(VECTOR.0));
    assert_eq!(status.fingerprint.as_deref(), Some(VECTOR.1));
    // Nothing detected in tests: the software baseline.
    assert_eq!(status.transcoding.video_encoders, ["libx264"]);
    assert_eq!(status.transcoding.audio_encoders, ["aac"]);

    admin
        .user_add(NewUser {
            username: "alice".into(),
            display_name: None,
            password: "battery staple".into(),
            role: Role::Consumer,
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
    let (_, admin, _dir) = admin_client("no-plex-sync").await;
    let err = admin.plex_watch_sync(true).await.unwrap_err();
    assert_eq!(code(err), error_code::NOT_CONFIGURED);
}

#[tokio::test]
async fn admin_methods_need_a_session() {
    let (addr, _dir) = server("no-session").await;
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
    let (addr, admin, _dir) = admin_client("forbidden").await;
    admin
        .user_add(NewUser {
            username: "alice".into(),
            display_name: None,
            password: "battery staple".into(),
            role: Role::Consumer,
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
    let (addr, admin, _dir) = admin_client("disable").await;
    admin
        .user_add(NewUser {
            username: "bob".into(),
            display_name: None,
            password: "battery staple".into(),
            role: Role::Admin,
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
    let (_, admin, _dir) = admin_client("logout").await;
    admin.logout().await.unwrap();
    let err = admin.user_list().await.unwrap_err();
    assert_eq!(code(err), error_code::UNAUTHENTICATED);
}

#[tokio::test]
async fn bad_input_is_named() {
    let (_, admin, _dir) = admin_client("bad-input").await;
    let short = admin
        .user_add(NewUser {
            username: "alice".into(),
            display_name: None,
            password: "short".into(),
            role: Role::Consumer,
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
            role: Role::Consumer,
        })
        .await
        .unwrap_err();
    assert_eq!(code(dup), error_code::USER_EXISTS);
}

#[tokio::test]
async fn setup_needs_the_code_from_the_log() {
    let (addr, _dir) = server("setup-code").await;
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
    let (addr, _dir) = server("setup-guess").await;
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
    let dir = state_dir("no-setup").await;
    let auth = Auth::new(Store::open(dir.as_ref()).await.unwrap());
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

/// Alice, a consumer, added by the admin; her own logged-in client.
async fn consumer(addr: SocketAddr, admin: &WsClient) -> (WsClient, String) {
    admin
        .user_add(NewUser {
            username: "alice".into(),
            display_name: None,
            password: "battery staple".into(),
            role: Role::Consumer,
        })
        .await
        .unwrap();
    let login = anonymous(addr)
        .await
        .login(creds("alice", "battery staple"))
        .await
        .unwrap();
    (with_token(addr, &login.token).await, login.token)
}

#[tokio::test]
async fn a_consumer_manages_their_own_account_and_nothing_else() {
    let (addr, admin, _dir) = admin_client("consumer").await;
    let (alice, _) = consumer(addr, &admin).await;
    assert_eq!(alice.who_am_i().await.unwrap().role, Role::Consumer);
    // Every admin.* call is refused.
    let forbidden = |r: Result<_, ClientError>| code(r.unwrap_err()) == error_code::FORBIDDEN;
    assert!(forbidden(alice.user_list().await.map(drop)));
    assert!(forbidden(
        alice
            .user_add(NewUser {
                username: "eve".into(),
                display_name: None,
                password: "battery staple".into(),
                role: Role::Admin,
            })
            .await
            .map(drop)
    ));
    assert!(forbidden(
        alice
            .user_set_role("alice".into(), Role::Admin)
            .await
            .map(drop)
    ));
    assert!(forbidden(
        alice.user_set_password(creds("root", "mine now!!")).await
    ));
    assert!(forbidden(alice.user_disable("root".into()).await.map(drop)));
    assert!(forbidden(alice.user_enable("root".into()).await));
    assert!(forbidden(alice.server_status().await.map(drop)));
    assert!(forbidden(alice.plex_watch_sync(true).await.map(drop)));

    // Her own password: only with the current one.
    let wrong = alice
        .set_password(PasswordChange {
            current_password: "guess".into(),
            new_password: "new password".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(code(wrong), error_code::INVALID_CREDENTIALS);
    alice
        .set_password(PasswordChange {
            current_password: "battery staple".into(),
            new_password: "new password".into(),
        })
        .await
        .unwrap();
    anonymous(addr)
        .await
        .login(creds("alice", "new password"))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_user_sees_and_ends_only_their_own_sessions() {
    let (addr, admin, _dir) = admin_client("own-sessions").await;
    let (alice, _) = consumer(addr, &admin).await;
    let second = anonymous(addr)
        .await
        .login(creds("alice", "battery staple"))
        .await
        .unwrap();
    let sessions = alice.sessions().await.unwrap();
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    assert_eq!(sessions.iter().filter(|s| s.current).count(), 1);
    let other = sessions.iter().find(|s| !s.current).unwrap().id.clone();
    // Not among root's sessions: root cannot end it.
    let refused = admin.revoke_session(other.clone()).await.unwrap_err();
    assert_eq!(code(refused), error_code::NO_SUCH_SESSION);
    alice.revoke_session(other).await.unwrap();
    let ended = with_token(addr, &second.token)
        .await
        .who_am_i()
        .await
        .unwrap_err();
    assert_eq!(code(ended), error_code::UNAUTHENTICATED);
    assert_eq!(alice.sessions().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_demoted_admin_is_refused_at_once_without_being_signed_out() {
    let (addr, root, _dir) = admin_client("demoted").await;
    let (alice, _) = consumer(addr, &root).await;
    root.user_set_role("alice".into(), Role::Admin)
        .await
        .unwrap();
    // The same session, now an admin's.
    alice.user_list().await.unwrap();
    root.user_set_role("alice".into(), Role::Consumer)
        .await
        .unwrap();
    let refused = alice.user_list().await.unwrap_err();
    assert_eq!(code(refused), error_code::FORBIDDEN);
    assert_eq!(alice.who_am_i().await.unwrap().username, "alice");
}

#[tokio::test]
async fn the_last_enabled_admin_cannot_be_demoted_or_disabled() {
    let (_, root, _dir) = admin_client("last-admin").await;
    let demote = root.user_set_role("root".into(), Role::Consumer).await;
    assert_eq!(code(demote.unwrap_err()), error_code::LAST_ADMIN);
    let disable = root.user_disable("root".into()).await;
    assert_eq!(code(disable.unwrap_err()), error_code::LAST_ADMIN);
    assert_eq!(root.who_am_i().await.unwrap().role, Role::Admin);
}

/// Alice signs in on her TV, as a client would: a device of hers.
async fn sign_in_on_tv(dir: &std::path::Path) -> String {
    let store = Store::open(dir).await.unwrap();
    let alice = store
        .password_login("alice")
        .await
        .unwrap()
        .expect("alice exists")
        .id;
    let tv = "ab".repeat(32);
    store
        .create_session(
            "tv-token-hash",
            alice,
            &spoolframe_server::store::sessions::SessionBinding::Endpoint {
                endpoint_id: tv.clone(),
                device_name: Some("Living room TV".into()),
            },
            i64::MAX / 2,
        )
        .await
        .unwrap();
    tv
}

#[tokio::test]
async fn a_user_sets_their_pin_and_where_it_is_asked_for_and_an_admin_clears_it() {
    let (addr, root, dir) = admin_client("pins").await;
    let (alice, _) = consumer(addr, &root).await;
    let tv = sign_in_on_tv(dir.as_ref()).await;

    // Her own PIN, from her signed-in session: 4 to 8 digits.
    let short = alice.pin_set("12".into()).await.unwrap_err();
    assert_eq!(code(short), error_code::INVALID_INPUT);
    alice.pin_set("4321".into()).await.unwrap();

    // Asked for on her TV, and off again: the session is all it takes.
    alice.set_pin_required(tv.clone(), true).await.unwrap();
    let devices = alice.devices().await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name.as_deref(), Some("Living room TV"));
    assert!(devices[0].pin_required);
    alice.set_pin_required(tv.clone(), false).await.unwrap();
    assert!(!alice.devices().await.unwrap()[0].pin_required);
    alice.set_pin_required(tv.clone(), true).await.unwrap();
    let unknown = alice
        .set_pin_required("cd".repeat(32), true)
        .await
        .unwrap_err();
    assert_eq!(code(unknown), error_code::NO_SUCH_DEVICE);

    // An admin sees it, and clears the PIN: asked for nowhere then.
    assert!(root.user_devices("alice".into()).await.unwrap()[0].pin_required);
    let forbidden = alice.user_clear_pin("alice".into()).await.unwrap_err();
    assert_eq!(code(forbidden), error_code::FORBIDDEN);
    root.user_clear_pin("alice".into()).await.unwrap();
    assert!(!alice.devices().await.unwrap()[0].pin_required);
    // Requiring it again needs a PIN first.
    let no_pin = root
        .user_set_pin_required("alice".into(), tv, true)
        .await
        .unwrap_err();
    assert_eq!(code(no_pin), error_code::INVALID_INPUT);
}
