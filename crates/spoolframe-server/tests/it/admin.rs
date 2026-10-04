//! The admin API over a real WebSocket, with the generated client.

use std::net::SocketAddr;

use jsonrpsee::core::ClientError;
use jsonrpsee::ws_client::HeaderMap;
use jsonrpsee::ws_client::{WsClient, WsClientBuilder};
use spoolframe_admin_api::{
    AdminApiClient, Credentials, MeApiClient, NewUser, PasswordChange, Role, SetupRequest,
    error_code,
};
use spoolframe_server::admin::{self, Admin, Origins, SetupCode, TrustedProxies};
use spoolframe_server::auth::Auth;
use spoolframe_server::catalog::LiveCatalog;
use spoolframe_server::passkeys::Passkeys;
use spoolframe_server::store::Store;

/// The setup code the test servers accept.
pub(crate) const CODE: &str = "K7QD-M2XP-9RTB";

/// The status of a server whose key is the first fingerprint test vector.
fn status() -> std::sync::Arc<admin::StatusSource> {
    let status = admin::StatusSource::new(
        spoolframe_server::network::ServerName::new("Den"),
        crate::support::transcoding(),
    );
    status.set_endpoint_id(VECTOR.0.to_owned());
    std::sync::Arc::new(status)
}

const VECTOR: (&str, &str) = spoolframe_protocol::fingerprint::TEST_VECTORS[0];

/// The admin API over `auth`, without Plex.
pub(crate) fn admin(auth: Auth, setup_code: Option<SetupCode>) -> Admin {
    Admin {
        auth,
        setup_code,
        status: status(),
        plex_sync: None,
        accounts: None,
        plex: Default::default(),
        cookies: Default::default(),
        settings: None,
        wizard: Default::default(),
        passkeys: std::sync::Arc::new(Passkeys::new(&Origins::default())),
        oidc: Default::default(),
    }
}

/// A directory of the test's own under a random name, removed when
/// dropped, so runs at once never share one.
pub(crate) async fn state_dir(test: &str) -> spoolframe_test_dir::TestDir {
    spoolframe_test_dir::TestDir::new(&format!("spoolframe-admin-{test}")).await
}

/// An admin API on a store in a directory kept for the test.
pub(crate) async fn server(test: &str) -> (SocketAddr, spoolframe_test_dir::TestDir) {
    let dir = state_dir(test).await;
    let auth = Auth::new(Store::open(dir.as_ref()).await.unwrap());
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        admin(auth, Some(SetupCode::new(CODE))),
        Origins::default(),
        TrustedProxies::default(),
    )
    .await
    .unwrap();
    // Runs for the life of the test process.
    std::mem::forget(handle);
    (addr, dir)
}

/// As [`server`], where browsers reach it at `public_url`, which is also
/// what passkeys are bound to.
pub(crate) async fn server_at(
    test: &str,
    public_url: &str,
) -> (SocketAddr, spoolframe_test_dir::TestDir) {
    server_at_behind(test, public_url, TrustedProxies::default()).await
}

/// As [`server_at`], with `proxies` believed about whom they carry.
pub(crate) async fn server_at_behind(
    test: &str,
    public_url: &str,
    proxies: TrustedProxies,
) -> (SocketAddr, spoolframe_test_dir::TestDir) {
    let dir = state_dir(test).await;
    let auth = Auth::new(Store::open(dir.as_ref()).await.unwrap());
    let origins = Origins::Exactly(public_url.to_owned());
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            passkeys: std::sync::Arc::new(Passkeys::new(&origins)),
            ..admin(auth, Some(SetupCode::new(CODE)))
        },
        origins,
        proxies,
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    (addr, dir)
}

/// The admin API beside a client endpoint: over the `auth` they share,
/// with browsers reaching it at `public_url`.
pub(crate) async fn serve_beside(auth: Auth, public_url: &str) -> SocketAddr {
    let origins = Origins::Exactly(public_url.to_owned());
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            passkeys: std::sync::Arc::new(Passkeys::new(&origins)),
            ..admin(auth, None)
        },
        origins,
        TrustedProxies::default(),
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    addr
}

/// As [`server`], behind a reverse proxy on this machine: connections from
/// loopback are the proxy's.
pub(crate) async fn server_behind_a_proxy(
    test: &str,
) -> (SocketAddr, spoolframe_test_dir::TestDir) {
    let dir = state_dir(test).await;
    let auth = Auth::new(Store::open(dir.as_ref()).await.unwrap());
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        admin(auth, Some(SetupCode::new(CODE))),
        Origins::default(),
        "127.0.0.1".parse().unwrap(),
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    (addr, dir)
}

pub(crate) async fn anonymous(addr: SocketAddr) -> WsClient {
    WsClientBuilder::default()
        .build(format!("ws://{addr}"))
        .await
        .unwrap()
}

pub(crate) async fn with_token(addr: SocketAddr, token: &str) -> WsClient {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    WsClientBuilder::default()
        .set_headers(headers)
        .build(format!("ws://{addr}"))
        .await
        .unwrap()
}

pub(crate) fn setup(code: &str, username: &str, password: &str) -> SetupRequest {
    SetupRequest {
        code: code.into(),
        username: username.into(),
        password: password.into(),
    }
}

pub(crate) fn creds(username: &str, password: &str) -> Credentials {
    Credentials {
        username: username.into(),
        password: password.into(),
    }
}

pub(crate) fn code(err: ClientError) -> i32 {
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
    let err = admin.plex_watch_sync(None, true).await.unwrap_err();
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
async fn failed_logins_as_one_user_do_not_lock_the_others_out() {
    let (addr, _admin, _dir) = admin_client("login-others").await;
    let client = anonymous(addr).await;
    for _ in 0..spoolframe_server::auth::MAX_FAILURES {
        let err = client
            .login(creds("mallory", "wrong guess"))
            .await
            .unwrap_err();
        assert_eq!(code(err), error_code::INVALID_CREDENTIALS);
    }
    // The name that was guessed at is limited.
    let err = client
        .login(creds("mallory", "wrong guess"))
        .await
        .unwrap_err();
    assert_eq!(code(err), error_code::RATE_LIMITED);
    // Everyone else on this machine, from the same address, is not.
    let root = client.login(creds("root", "correct horse")).await.unwrap();
    assert_eq!(root.role, Role::Admin);
}

#[tokio::test]
async fn a_server_started_with_users_offers_no_setup() {
    let dir = state_dir("no-setup").await;
    let auth = Auth::new(Store::open(dir.as_ref()).await.unwrap());
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        admin(auth, None),
        Origins::default(),
        TrustedProxies::default(),
    )
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
    assert!(forbidden(alice.plex_watch_sync(None, true).await.map(drop)));

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
                platform: spoolframe_admin_api::Platform::Tvos,
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
    assert_eq!(
        devices[0].platform,
        Some(spoolframe_admin_api::Platform::Tvos)
    );
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

/// The admin API over `auth`, with Plex at `url`.
async fn server_with_plex(test: &str, url: &str) -> (SocketAddr, spoolframe_test_dir::TestDir) {
    let dir = state_dir(test).await;
    let auth = Auth::new(Store::open(dir.as_ref()).await.unwrap());
    let plex = spoolframe_plex::PlexCatalog::new(url.parse().unwrap(), None).unwrap();
    let (addr, handle) = admin::serve(
        "127.0.0.1:0".parse().unwrap(),
        Admin {
            plex: std::sync::Arc::new(LiveCatalog::new(Some(std::sync::Arc::new(
                spoolframe_server::catalog::Composite::single("Tower", plex),
            )))),
            ..admin(auth, Some(SetupCode::new(CODE)))
        },
        Origins::default(),
        TrustedProxies::default(),
    )
    .await
    .unwrap();
    std::mem::forget(handle);
    (addr, dir)
}

async fn root_on(addr: SocketAddr) -> WsClient {
    let setup = anonymous(addr)
        .await
        .setup(setup(CODE, "root", "correct horse"))
        .await
        .unwrap();
    with_token(addr, &setup.token).await
}

#[tokio::test]
async fn users_show_when_they_were_last_seen() {
    let (addr, root, _dir) = admin_client("last-seen").await;
    root.user_add(NewUser {
        username: "bob".into(),
        display_name: None,
        password: "battery staple".into(),
        role: Role::Consumer,
    })
    .await
    .unwrap();
    let users = root.user_list().await.unwrap();
    let seen = |name: &str| {
        users
            .iter()
            .find(|u| u.username == name)
            .unwrap()
            .last_seen_at
    };
    assert!(seen("root").is_some());
    assert_eq!(seen("bob"), None);
    let _ = addr;
}

/// The user page's PIN section: whether there is a PIN to clear.
#[tokio::test]
async fn the_user_list_says_who_has_a_pin() {
    let (addr, root, _dir) = admin_client("user-has-pin").await;
    let (alice, _) = consumer(addr, &root).await;
    let has_pin = |users: &[spoolframe_admin_api::UserInfo]| {
        users
            .iter()
            .find(|u| u.username == "alice")
            .unwrap()
            .has_pin
    };
    assert!(!has_pin(&root.user_list().await.unwrap()));
    alice.pin_set("4321".into()).await.unwrap();
    assert!(has_pin(&root.user_list().await.unwrap()));
    root.user_clear_pin("alice".into()).await.unwrap();
    assert!(!has_pin(&root.user_list().await.unwrap()));
}

/// The users page's columns, from the list itself: a row per user, not a
/// call per user.
#[tokio::test]
async fn the_user_list_counts_each_user_s_devices_and_live_sessions() {
    let (addr, root, dir) = admin_client("user-counts").await;
    let _alice = consumer(addr, &root).await;
    let tv = sign_in_on_tv(dir.as_ref()).await;
    // A second session on the TV: still one device.
    let store = Store::open(dir.as_ref()).await.unwrap();
    let alice_id = store.password_login("alice").await.unwrap().unwrap().id;
    store
        .create_session(
            "tv-token-hash-2",
            alice_id,
            &spoolframe_server::store::sessions::SessionBinding::Endpoint {
                endpoint_id: tv,
                device_name: Some("Living room TV".into()),
                platform: spoolframe_admin_api::Platform::Tvos,
            },
            i64::MAX / 2,
        )
        .await
        .unwrap();
    let counts = |users: &[spoolframe_admin_api::UserInfo], name: &str| {
        let user = users.iter().find(|u| u.username == name).unwrap();
        (user.device_count, user.session_count)
    };

    let users = root.user_list().await.unwrap();
    // Her browser and the TV twice; the TV is her one device.
    assert_eq!(counts(&users, "alice"), (1, 3));
    assert_eq!(counts(&users, "root"), (0, 1));

    // A revoked session is not live; the device it was on is still hers.
    let sessions = root.user_sessions("alice".into()).await.unwrap();
    let browser = sessions.iter().find(|s| s.endpoint_id.is_none()).unwrap();
    // The web UI says "Apple TV app" for the TV's, "Web or CLI" for hers.
    assert_eq!(browser.platform, None);
    assert!(
        sessions
            .iter()
            .filter(|s| s.endpoint_id.is_some())
            .all(|s| s.platform == Some(spoolframe_admin_api::Platform::Tvos))
    );
    root.user_revoke_session("alice".into(), browser.id.clone())
        .await
        .unwrap();
    assert_eq!(counts(&root.user_list().await.unwrap(), "alice"), (1, 2));
}

#[tokio::test]
async fn an_admin_lists_and_ends_another_user_s_sessions() {
    let (addr, root, _dir) = admin_client("user-sessions").await;
    root.user_add(NewUser {
        username: "bob".into(),
        display_name: None,
        password: "battery staple".into(),
        role: Role::Consumer,
    })
    .await
    .unwrap();
    let login = anonymous(addr)
        .await
        .login(creds("bob", "battery staple"))
        .await
        .unwrap();
    let bob = with_token(addr, &login.token).await;
    // Bob may not look at anyone's, his own included, through admin.*.
    assert_eq!(
        code(bob.user_sessions("bob".into()).await.unwrap_err()),
        error_code::FORBIDDEN
    );

    let sessions = root.user_sessions("bob".into()).await.unwrap();
    assert_eq!(sessions.len(), 1);
    // A CLI session: no device.
    assert_eq!(sessions[0].endpoint_id, None);
    assert!(!sessions[0].current);
    assert_eq!(
        code(
            root.user_revoke_session("bob".into(), "nope".into())
                .await
                .unwrap_err()
        ),
        error_code::NO_SUCH_SESSION
    );
    root.user_revoke_session("bob".into(), sessions[0].id.clone())
        .await
        .unwrap();
    assert_eq!(
        code(bob.who_am_i().await.unwrap_err()),
        error_code::UNAUTHENTICATED
    );
    // Root's own is untouched.
    root.who_am_i().await.unwrap();
}

#[tokio::test]
async fn plex_down_is_an_answer_not_an_error() {
    // Nothing listens on port 9 here.
    let (addr, _dir) = server_with_plex("plex-down", "http://127.0.0.1:9").await;
    let root = root_on(addr).await;
    let status = root.plex_status().await.unwrap();
    assert!(!status.reachable);
    assert!(status.error.is_some());
    assert!(status.libraries.is_empty());
    // Where it was tried, for the page to say.
    assert_eq!(status.url.as_deref(), Some("http://127.0.0.1:9"));
}

#[tokio::test]
async fn plex_status_names_the_server_and_counts_each_library() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let pms = MockServer::start().await;
    let json = |body: serde_json::Value| ResponseTemplate::new(200).set_body_json(body);
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(json(
            serde_json::json!({ "MediaContainer": { "friendlyName": "Den" } }),
        ))
        .mount(&pms)
        .await;
    Mock::given(method("GET"))
        .and(path("/library/sections"))
        .respond_with(json(serde_json::json!({ "MediaContainer": { "Directory": [
            { "key": "1", "type": "movie", "title": "Movies" },
            { "key": "2", "type": "show", "title": "TV" },
        ] } })))
        .mount(&pms)
        .await;
    for (key, total) in [("1", 770), ("2", 41)] {
        Mock::given(method("GET"))
            .and(path(format!("/library/sections/{key}/all")))
            .respond_with(json(
                serde_json::json!({ "MediaContainer": { "totalSize": total } }),
            ))
            .mount(&pms)
            .await;
    }
    let (addr, _dir) = server_with_plex("plex-up", &pms.uri()).await;
    let root = root_on(addr).await;
    let status = root.plex_status().await.unwrap();
    assert!(status.reachable, "{status:?}");
    assert_eq!(status.server_name.as_deref(), Some("Den"));
    assert_eq!(status.url.as_deref(), Some(pms.uri().as_str()));
    let counts: Vec<_> = status
        .libraries
        .iter()
        .map(|l| (l.title.as_str(), l.items))
        .collect();
    assert_eq!(counts, [("Movies", 770), ("TV", 41)]);
}

#[tokio::test]
async fn without_plex_its_methods_say_so() {
    let (_addr, root, _dir) = admin_client("no-plex").await;
    assert_eq!(
        code(root.plex_status().await.unwrap_err()),
        error_code::NOT_CONFIGURED
    );
}
