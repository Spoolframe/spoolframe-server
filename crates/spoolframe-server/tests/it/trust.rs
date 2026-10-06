//! Trusted devices (P4-01): a device the user chose to remember starts a
//! session for them without a password, until they sign out there, their
//! password changes, they are disabled, someone ends their session there,
//! or the device goes unused too long. Never from another device, never as
//! another user, and never past the PIN.

use crate::support;

use spoolframe_protocol::{
    DeviceSignInRequest, Error, LoginPassword, Reply, Request, Response, SessionId, TrustedProfile,
};
use spoolframe_types::Platform;
use support::{ServerOptions, TestClient, TestServer, fresh_dir};

const PASSWORD: &str = "correct horse";
const PIN: &str = "4321";

async fn server(test: &str) -> TestServer {
    let server = TestServer::start_with(fresh_dir(test).await, ServerOptions::default()).await;
    server.add_user("alice", PASSWORD, Some("Alice")).await;
    server.add_user("bob", "battery staple", None).await;
    server
}

/// A device that said Hello.
async fn device(server: &TestServer, name: &str) -> TestClient {
    let client = server.client().await;
    client.hello_as(Some(name)).await;
    client
}

async fn sign_in(client: &TestClient, username: &str, password: &str, remember: bool) -> Reply {
    let request = Request::LoginPassword(LoginPassword {
        username: username.into(),
        password: password.into(),
        remember,
    });
    match client.call(None, request).await {
        Response::Ok(reply) => reply,
        other => panic!("sign-in as {username} failed: {other:?}"),
    }
}

/// Alice signed in on `client` with "Remember this device" ticked.
async fn remembered(client: &TestClient) -> SessionId {
    match sign_in(client, "alice", PASSWORD, true).await {
        Reply::LoggedIn {
            session,
            remembered,
        } => {
            assert!(remembered);
            session
        }
        other => panic!("{other:?}"),
    }
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

async fn trusted(client: &TestClient) -> Vec<TrustedProfile> {
    match client.call(None, Request::TrustedProfiles).await {
        Response::Ok(Reply::TrustedProfiles { profiles }) => profiles,
        other => panic!("{other:?}"),
    }
}

async fn trusted_names(client: &TestClient) -> Vec<String> {
    trusted(client)
        .await
        .into_iter()
        .map(|p| p.username)
        .collect()
}

async fn resume(client: &TestClient, user_id: &str) -> Response {
    client
        .call(
            None,
            Request::ResumeProfile {
                user_id: user_id.into(),
            },
        )
        .await
}

async fn resumed(client: &TestClient, user_id: &str) -> SessionId {
    match resume(client, user_id).await {
        Response::Ok(Reply::LoggedIn {
            session,
            remembered,
        }) => {
            assert!(remembered);
            session
        }
        other => panic!("not resumed: {other:?}"),
    }
}

async fn user_id(server: &TestServer, username: &str) -> String {
    let user = server
        .store
        .password_login(username)
        .await
        .unwrap()
        .unwrap();
    format!("local:{}", user.id)
}

/// The server's database, as only the server opens it: to age rows as
/// time would.
async fn sql(server: &TestServer, query: &'static str) {
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(server.dir.join("spoolframe.db")),
    )
    .await
    .unwrap();
    sqlx::query(query).execute(&pool).await.unwrap();
    pool.close().await;
}

/// Every session as if it expired an hour ago.
async fn expire_sessions(server: &TestServer) {
    sql(
        server,
        "UPDATE sessions SET expires_at = expires_at - 100000000",
    )
    .await;
}

/// Every session as if signed in an hour ago: past the recent sign-in.
async fn age_sign_ins(server: &TestServer) {
    sql(
        server,
        "UPDATE sessions SET authenticated_at = authenticated_at - 3600",
    )
    .await;
}

fn set_trust(device: Option<String>, trusted: bool, auto_login: bool) -> Request {
    Request::SetDeviceTrust {
        device,
        trusted,
        auto_login,
    }
}

#[tokio::test]
async fn a_sign_in_that_remembers_trusts_the_device_and_one_that_does_not_does_not() {
    let server = server("trust-sign-in").await;
    let tv = device(&server, "Living room TV").await;
    match sign_in(&tv, "alice", PASSWORD, false).await {
        Reply::LoggedIn { remembered, .. } => assert!(!remembered),
        other => panic!("{other:?}"),
    }
    assert!(trusted(&tv).await.is_empty());
    remembered(&tv).await;
    assert_eq!(
        trusted(&tv).await,
        [TrustedProfile {
            user_id: user_id(&server, "alice").await,
            username: "alice".into(),
            display_name: "Alice".into(),
            pin_required: false,
            auto_login: false,
        }]
    );
    server.stop().await;
}

#[tokio::test]
async fn after_the_session_expires_the_device_resumes_and_no_other_device_can() {
    let server = server("trust-resume").await;
    let tv = device(&server, "Living room TV").await;
    let old = remembered(&tv).await;
    expire_sessions(&server).await;
    assert_eq!(
        err(&tv, &old, Request::WhoAmI).await,
        Error::Unauthenticated
    );
    let alice = user_id(&server, "alice").await;
    let session = resumed(&tv, &alice).await;
    match ok(&tv, &session, Request::WhoAmI).await {
        Reply::WhoAmI(user) => assert_eq!(user.username, "alice"),
        other => panic!("{other:?}"),
    }
    // Trust is the device's key: another device is a stranger to it.
    let laptop = device(&server, "Laptop").await;
    assert!(trusted(&laptop).await.is_empty());
    assert_eq!(
        resume(&laptop, &alice).await,
        Response::Err(Error::Unauthenticated)
    );
    // And the resumed session is bound to the TV.
    assert_eq!(
        err(&laptop, &session, Request::WhoAmI).await,
        Error::Unauthenticated
    );
    server.stop().await;
}

#[tokio::test]
async fn a_device_trusted_for_one_user_resumes_no_other() {
    let server = server("trust-cross-user").await;
    let tv = device(&server, "Living room TV").await;
    remembered(&tv).await;
    // Bob has signed in here too, without remembering it.
    sign_in(&tv, "bob", "battery staple", false).await;
    let bob = user_id(&server, "bob").await;
    for id in [
        bob.as_str(),
        "local:not-a-uuid",
        "0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c",
        "local:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c",
        "",
    ] {
        assert_eq!(
            resume(&tv, id).await,
            Response::Err(Error::Unauthenticated),
            "{id:?}"
        );
    }
    assert_eq!(trusted_names(&tv).await, ["alice"]);
    server.stop().await;
}

#[tokio::test]
async fn the_device_lists_only_its_own_trust() {
    let server = server("trust-own-rows").await;
    let tv = device(&server, "Living room TV").await;
    let laptop = device(&server, "Laptop").await;
    remembered(&tv).await;
    match sign_in(&laptop, "bob", "battery staple", true).await {
        Reply::LoggedIn { remembered, .. } => assert!(remembered),
        other => panic!("{other:?}"),
    }
    assert_eq!(trusted_names(&tv).await, ["alice"]);
    assert_eq!(trusted_names(&laptop).await, ["bob"]);
    server.stop().await;
}

#[tokio::test]
async fn signing_out_forgets_the_device() {
    let server = server("trust-logout").await;
    let tv = device(&server, "Living room TV").await;
    let session = remembered(&tv).await;
    assert_eq!(ok(&tv, &session, Request::Logout).await, Reply::LoggedOut);
    let tv = tv.reconnect(&server).await;
    tv.hello().await;
    assert!(trusted(&tv).await.is_empty());
    assert_eq!(
        resume(&tv, &user_id(&server, "alice").await).await,
        Response::Err(Error::Unauthenticated)
    );
    server.stop().await;
}

/// PX-09: a resume must not undo a revocation. An admin ending the
/// session of a stolen TV ends its trust too.
#[tokio::test]
async fn a_revoked_session_does_not_come_back_as_a_resume() {
    let server = server("trust-revoked").await;
    let tv = device(&server, "Living room TV").await;
    let session = remembered(&tv).await;
    let alice = server
        .store
        .password_login("alice")
        .await
        .unwrap()
        .unwrap()
        .id;
    let hash = server
        .store
        .list_sessions(alice)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .token_hash;
    server.store.revoke_session(&hash).await.unwrap();
    let tv = tv.reconnect(&server).await;
    tv.hello().await;
    assert_eq!(
        err(&tv, &session, Request::WhoAmI).await,
        Error::Unauthenticated
    );
    assert!(trusted(&tv).await.is_empty());
    assert_eq!(
        resume(&tv, &format!("local:{alice}")).await,
        Response::Err(Error::Unauthenticated)
    );
    server.stop().await;
}

#[tokio::test]
async fn a_password_change_or_disabling_the_user_ends_their_trust() {
    let server = server("trust-password").await;
    let tv = device(&server, "Living room TV").await;
    let laptop = device(&server, "Laptop").await;
    remembered(&tv).await;
    remembered(&laptop).await;
    let alice = user_id(&server, "alice").await;
    let phc = spoolframe_server::auth::hash_password("another horse")
        .await
        .unwrap();
    server
        .store
        .set_password("alice", &phc, None)
        .await
        .unwrap();
    for client in [&tv, &laptop] {
        let client = client.reconnect(&server).await;
        client.hello().await;
        assert!(trusted(&client).await.is_empty());
        assert_eq!(
            resume(&client, &alice).await,
            Response::Err(Error::Unauthenticated)
        );
    }

    // Disabled: refused, and enabling them again brings no trust back.
    let tv = tv.reconnect(&server).await;
    tv.hello().await;
    match sign_in(&tv, "alice", "another horse", true).await {
        Reply::LoggedIn { remembered, .. } => assert!(remembered),
        other => panic!("{other:?}"),
    }
    server.store.disable_user("alice").await.unwrap();
    let tv = tv.reconnect(&server).await;
    tv.hello().await;
    assert_eq!(
        resume(&tv, &alice).await,
        Response::Err(Error::Unauthenticated)
    );
    server.store.enable_user("alice").await.unwrap();
    assert_eq!(
        resume(&tv, &alice).await,
        Response::Err(Error::Unauthenticated)
    );
    assert!(trusted(&tv).await.is_empty());
    server.stop().await;
}

#[tokio::test]
async fn trust_unused_past_the_limit_is_refused_and_removed() {
    let server = server("trust-lapsed").await;
    let tv = device(&server, "Living room TV").await;
    remembered(&tv).await;
    expire_sessions(&server).await;
    sql(
        &server,
        "UPDATE trusted_endpoints SET last_used_at = last_used_at - 181 * 86400",
    )
    .await;
    assert_eq!(
        resume(&tv, &user_id(&server, "alice").await).await,
        Response::Err(Error::Unauthenticated)
    );
    assert!(trusted(&tv).await.is_empty());
    server.stop().await;
}

/// The PIN guards a shared device whatever made the session.
#[tokio::test]
async fn a_resumed_profile_on_a_device_that_asks_for_the_pin_starts_locked() {
    let server = server("trust-pin").await;
    let tv = device(&server, "Living room TV").await;
    let session = remembered(&tv).await;
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
    expire_sessions(&server).await;
    let tv = tv.reconnect(&server).await;
    tv.hello().await;
    assert!(trusted(&tv).await[0].pin_required);
    let session = resumed(&tv, &user_id(&server, "alice").await).await;
    match ok(&tv, &session, Request::WhoAmI).await {
        Reply::WhoAmI(user) => assert!(user.locked),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        err(&tv, &session, Request::Libraries).await,
        Error::ProfileLocked
    );
    ok(&tv, &session, Request::UnlockProfile { pin: PIN.into() }).await;
    ok(&tv, &session, Request::Libraries).await;
    server.stop().await;
}

#[tokio::test]
async fn auto_login_turns_the_pin_off_and_the_pin_turns_auto_login_off() {
    let server = server("trust-auto-pin").await;
    let tv = device(&server, "Living room TV").await;
    let session = remembered(&tv).await;
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
    ok(&tv, &session, set_trust(None, true, true)).await;
    let profile = &trusted(&tv).await[0];
    assert!(profile.auto_login);
    assert!(!profile.pin_required);
    ok(
        &tv,
        &session,
        Request::SetPinRequired {
            device: None,
            required: true,
        },
    )
    .await;
    let profile = &trusted(&tv).await[0];
    assert!(!profile.auto_login);
    assert!(profile.pin_required);
    server.stop().await;
}

#[tokio::test]
async fn auto_login_for_a_second_user_turns_it_off_for_the_first() {
    let server = server("trust-auto-one").await;
    let tv = device(&server, "Living room TV").await;
    let alice = remembered(&tv).await;
    let bob = match sign_in(&tv, "bob", "battery staple", true).await {
        Reply::LoggedIn { session, .. } => session,
        other => panic!("{other:?}"),
    };
    ok(&tv, &alice, set_trust(None, true, true)).await;
    ok(&tv, &bob, set_trust(None, true, true)).await;
    let auto: Vec<_> = trusted(&tv)
        .await
        .into_iter()
        .map(|p| (p.username, p.auto_login))
        .collect();
    assert_eq!(auto, [("alice".into(), false), ("bob".into(), true)]);
    server.stop().await;
}

#[tokio::test]
async fn a_client_trusts_only_itself_and_only_after_a_recent_sign_in() {
    let server = server("trust-client-rules").await;
    let tv = device(&server, "Living room TV").await;
    let laptop = device(&server, "Laptop").await;
    let on_tv = match sign_in(&tv, "alice", PASSWORD, false).await {
        Reply::LoggedIn { session, .. } => session,
        other => panic!("{other:?}"),
    };
    let on_laptop = remembered(&laptop).await;
    let tv_id = tv.endpoint.id().to_string();
    // Another device: only the web UI trusts it.
    assert!(matches!(
        err(
            &laptop,
            &on_laptop,
            set_trust(Some(tv_id.clone()), true, false)
        )
        .await,
        Error::InvalidInput { .. }
    ));
    assert!(trusted(&tv).await.is_empty());
    // Auto-login needs trust.
    assert!(matches!(
        err(&tv, &on_tv, set_trust(None, false, true)).await,
        Error::InvalidInput { .. }
    ));
    // A session left signed in grants no trust.
    age_sign_ins(&server).await;
    assert_eq!(
        err(&tv, &on_tv, set_trust(None, true, false)).await,
        Error::RecentSignInRequired
    );
    assert!(trusted(&tv).await.is_empty());
    // Nor ends another device's sessions.
    let laptop_id = laptop.endpoint.id().to_string();
    assert_eq!(
        err(
            &tv,
            &on_tv,
            set_trust(Some(laptop_id.clone()), false, false)
        )
        .await,
        Error::RecentSignInRequired
    );
    // Confirmed, it does.
    ok(
        &tv,
        &on_tv,
        Request::ConfirmPassword(spoolframe_protocol::ConfirmPassword {
            password: PASSWORD.into(),
        }),
    )
    .await;
    ok(&tv, &on_tv, set_trust(None, true, false)).await;
    assert_eq!(trusted_names(&tv).await, ["alice"]);
    ok(&tv, &on_tv, set_trust(Some(laptop_id), false, false)).await;
    // Its connection closes with its last session; it comes back a stranger.
    let laptop = laptop.reconnect(&server).await;
    laptop.hello().await;
    assert!(trusted(&laptop).await.is_empty());
    assert_eq!(
        err(&laptop, &on_laptop, Request::WhoAmI).await,
        Error::Unauthenticated
    );
    // A device that is not hers.
    let stranger = device(&server, "Stranger").await;
    assert!(matches!(
        err(
            &tv,
            &on_tv,
            set_trust(Some(stranger.endpoint.id().to_string()), false, false)
        )
        .await,
        Error::NotFound { .. }
    ));
    server.stop().await;
}

/// Stopping trust on this device takes away only, so a session left
/// signed in may; it ends the user's sessions here.
#[tokio::test]
async fn stopping_trust_here_needs_no_recent_sign_in_and_ends_the_session() {
    let server = server("trust-off-here").await;
    let tv = device(&server, "Living room TV").await;
    let session = remembered(&tv).await;
    age_sign_ins(&server).await;
    ok(&tv, &session, set_trust(None, false, false)).await;
    let tv = tv.reconnect(&server).await;
    tv.hello().await;
    assert!(trusted(&tv).await.is_empty());
    assert_eq!(
        err(&tv, &session, Request::WhoAmI).await,
        Error::Unauthenticated
    );
    server.stop().await;
}

#[tokio::test]
async fn the_devices_list_says_which_are_trusted() {
    let server = server("trust-devices").await;
    let tv = device(&server, "Living room TV").await;
    let session = remembered(&tv).await;
    ok(&tv, &session, set_trust(None, true, true)).await;
    match ok(&tv, &session, Request::ListDevices).await {
        Reply::Devices { devices } => {
            assert_eq!(devices.len(), 1);
            assert!(devices[0].trusted);
            assert!(devices[0].auto_login);
        }
        other => panic!("{other:?}"),
    }
    server.stop().await;
}

/// A TV that asked to be remembered, waiting on its code.
async fn tv_asks(
    server: &TestServer,
    remember: bool,
) -> (TestClient, tokio::task::JoinHandle<Response>, String) {
    let tv = server.client().await;
    tv.hello_from(Platform::Tvos, Some("Living room TV"), None)
        .await;
    let (request, code) = match tv.call(None, Request::BeginDeviceSignIn { remember }).await {
        Response::Ok(Reply::DeviceSignInStarted { request, code, .. }) => (request, code),
        other => panic!("{other:?}"),
    };
    let waiting = {
        let tv = tv.clone();
        tokio::spawn(async move { tv.call(None, Request::AwaitDeviceSignIn { request }).await })
    };
    (tv, waiting, code)
}

/// The approver decides, and cannot trust a device that did not ask.
#[tokio::test]
async fn the_approver_of_a_device_sign_in_decides_whether_it_is_remembered() {
    let server = server("trust-approval").await;
    let phone = device(&server, "Alice's iPhone").await;
    let approver = match sign_in(&phone, "alice", PASSWORD, false).await {
        Reply::LoggedIn { session, .. } => session,
        other => panic!("{other:?}"),
    };
    for (asked, approved, expected) in [
        (true, false, false),
        (true, true, true),
        (false, true, false),
    ] {
        let (tv, waiting, code) = tv_asks(&server, asked).await;
        match ok(
            &phone,
            &approver,
            Request::LookupDeviceSignIn { code: code.clone() },
        )
        .await
        {
            Reply::DeviceSignInRequest(DeviceSignInRequest { remember, .. }) => {
                assert_eq!(remember, asked);
            }
            other => panic!("{other:?}"),
        }
        ok(
            &phone,
            &approver,
            Request::ApproveDeviceSignIn {
                code,
                remember: approved,
            },
        )
        .await;
        match waiting.await.unwrap() {
            Response::Ok(Reply::LoggedIn { remembered, .. }) => {
                assert_eq!(remembered, expected, "asked {asked}, approved {approved}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            trusted_names(&tv).await.len(),
            usize::from(expected),
            "asked {asked}, approved {approved}"
        );
        tv.close().await;
    }
    server.stop().await;
}

/// Inside a namespace with only loopback: no route out (§24F).
const INSIDE: &str = "SPOOLFRAME_TEST_NO_ROUTE_OUT";

/// Run `test` again in a new user and network namespace whose only
/// interface is loopback, and assert it passed there. `true` in the copy
/// inside, which runs the body; `false` outside.
fn offline(test: &str) -> bool {
    if std::env::var_os(INSIDE).is_some() {
        return true;
    }
    let exe = std::env::current_exe().unwrap();
    let (_, module) = module_path!().split_once("::").unwrap();
    let test = format!("{module}::{test}");
    let output = std::process::Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "sh", "-ec"])
        .arg("ip link set lo up; exec \"$0\" --exact \"$1\" --nocapture")
        .arg(&exe)
        .arg(&test)
        .env(INSIDE, "1")
        .output()
        .expect("unshare (util-linux) runs");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{test} with no route out:\n{log}");
    assert!(log.contains("1 passed"), "{test} did not run:\n{log}");
    false
}

#[tokio::test]
async fn resuming_needs_no_route_out() {
    if !offline("resuming_needs_no_route_out") {
        return;
    }
    let server = server("trust-offline").await;
    let tv = device(&server, "Living room TV").await;
    remembered(&tv).await;
    expire_sessions(&server).await;
    resumed(&tv, &user_id(&server, "alice").await).await;
    server.stop().await;
}

/// PX-09: a resume is no proof of who is at the device. A resumed session
/// makes no change that needs a recent sign-in until its user confirms.
#[tokio::test]
async fn a_resumed_session_is_not_a_recent_sign_in() {
    let server = server("trust-not-recent").await;
    let tv = device(&server, "Living room TV").await;
    remembered(&tv).await;
    expire_sessions(&server).await;
    let session = resumed(&tv, &user_id(&server, "alice").await).await;
    assert_eq!(
        err(&tv, &session, set_trust(None, true, true)).await,
        Error::RecentSignInRequired
    );
    assert_eq!(
        err(&tv, &session, Request::BeginPasskeyRegistration).await,
        Error::RecentSignInRequired
    );
    assert_eq!(
        err(
            &tv,
            &session,
            Request::SetPinRequired {
                device: None,
                required: false,
            }
        )
        .await,
        Error::RecentSignInRequired
    );
    ok(
        &tv,
        &session,
        Request::ConfirmPassword(spoolframe_protocol::ConfirmPassword {
            password: PASSWORD.into(),
        }),
    )
    .await;
    ok(&tv, &session, set_trust(None, true, true)).await;
    server.stop().await;
}

/// The web UI's side (P4-03 builds on it): over the admin API beside the
/// server, as a browser reaches it.
mod web {
    use super::*;
    use spoolframe_admin_api::{AdminApiClient, Credentials, MeApiClient, error_code};

    const WEB: &str = "https://media.example.org";

    async fn on_the_web(
        web: std::net::SocketAddr,
        username: &str,
        password: &str,
    ) -> jsonrpsee::ws_client::WsClient {
        let login = crate::admin::anonymous(web)
            .await
            .login(crate::admin::creds(username, password))
            .await
            .unwrap();
        crate::admin::with_token(web, &login.token).await
    }

    async fn make_admin(server: &TestServer, username: &str) {
        server
            .store
            .set_role(username, spoolframe_types::Role::Admin)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_user_trusts_a_device_they_used_from_the_web_after_a_recent_sign_in() {
        let server = server("trust-web-me").await;
        let web = crate::admin::serve_beside(server.auth.clone(), WEB).await;
        let tv = device(&server, "Living room TV").await;
        sign_in(&tv, "alice", PASSWORD, false).await;
        let tv_id = tv.endpoint.id().to_string();
        let alice = on_the_web(web, "alice", PASSWORD).await;

        alice
            .set_device_trust(tv_id.clone(), true, false)
            .await
            .unwrap();
        let devices = alice.devices().await.unwrap();
        assert!(devices[0].trusted && !devices[0].auto_login, "{devices:?}");
        assert_eq!(trusted_names(&tv).await, ["alice"]);
        // Auto-login needs trust; a device she never used is not hers.
        assert_eq!(
            crate::admin::code(
                alice
                    .set_device_trust(tv_id.clone(), false, true)
                    .await
                    .unwrap_err()
            ),
            error_code::INVALID_INPUT
        );
        let laptop = device(&server, "Laptop").await;
        assert_eq!(
            crate::admin::code(
                alice
                    .set_device_trust(laptop.endpoint.id().to_string(), true, false)
                    .await
                    .unwrap_err()
            ),
            error_code::NO_SUCH_DEVICE
        );
        // A user's web session cannot trust for another user.
        assert_eq!(
            crate::admin::code(
                alice
                    .user_set_device_trust("bob".into(), tv_id.clone(), true, false)
                    .await
                    .unwrap_err()
            ),
            error_code::FORBIDDEN
        );

        // Past the recent sign-in, neither way.
        age_sign_ins(&server).await;
        for trusted in [true, false] {
            assert_eq!(
                crate::admin::code(
                    alice
                        .set_device_trust(tv_id.clone(), trusted, false)
                        .await
                        .unwrap_err()
                ),
                error_code::RECENT_SIGN_IN_REQUIRED
            );
        }
        // Confirmed, she stops trusting it: its sessions end there too.
        alice.confirm_password(PASSWORD.into()).await.unwrap();
        alice
            .set_device_trust(tv_id.clone(), false, false)
            .await
            .unwrap();
        let tv = tv.reconnect(&server).await;
        tv.hello().await;
        assert!(trusted(&tv).await.is_empty());
        assert!(
            alice
                .sessions()
                .await
                .unwrap()
                .iter()
                .all(|s| s.endpoint_id.as_deref() != Some(tv_id.as_str()))
        );
        server.stop().await;
    }

    #[tokio::test]
    async fn an_admin_trusts_another_user_s_device_and_a_reset_password_ends_it() {
        let server = server("trust-web-admin").await;
        make_admin(&server, "bob").await;
        let web = crate::admin::serve_beside(server.auth.clone(), WEB).await;
        let tv = device(&server, "Living room TV").await;
        sign_in(&tv, "alice", PASSWORD, false).await;
        let tv_id = tv.endpoint.id().to_string();
        let root = on_the_web(web, "bob", "battery staple").await;

        root.user_set_device_trust("alice".into(), tv_id.clone(), true, true)
            .await
            .unwrap();
        let devices = root.user_devices("alice".into()).await.unwrap();
        assert!(devices[0].trusted && devices[0].auto_login, "{devices:?}");
        assert_eq!(trusted_names(&tv).await, ["alice"]);

        root.user_set_password(Credentials {
            username: "alice".into(),
            password: "another horse".into(),
        })
        .await
        .unwrap();
        let tv = tv.reconnect(&server).await;
        tv.hello().await;
        assert!(trusted(&tv).await.is_empty());
        assert!(!root.user_devices("alice".into()).await.unwrap()[0].trusted);

        // Stale, an admin session trusts nothing.
        age_sign_ins(&server).await;
        assert_eq!(
            crate::admin::code(
                root.user_set_device_trust("alice".into(), tv_id, true, false)
                    .await
                    .unwrap_err()
            ),
            error_code::RECENT_SIGN_IN_REQUIRED
        );
        server.stop().await;
    }

    #[tokio::test]
    async fn disabling_a_user_from_the_web_ends_their_trust() {
        let server = server("trust-web-disable").await;
        make_admin(&server, "bob").await;
        let web = crate::admin::serve_beside(server.auth.clone(), WEB).await;
        let tv = device(&server, "Living room TV").await;
        remembered(&tv).await;
        let root = on_the_web(web, "bob", "battery staple").await;
        root.user_disable("alice".into()).await.unwrap();
        root.user_enable("alice".into()).await.unwrap();
        let tv = tv.reconnect(&server).await;
        tv.hello().await;
        assert!(trusted(&tv).await.is_empty());
        server.stop().await;
    }

    /// The web approval page (P4-03): approved as is, a TV that asked is
    /// remembered; unticked, it signs in just this once.
    #[tokio::test]
    async fn a_web_approval_decides_whether_the_tv_is_remembered() {
        let server = server("trust-web-approve").await;
        let web = crate::admin::serve_beside(server.auth.clone(), WEB).await;
        let alice = on_the_web(web, "alice", PASSWORD).await;
        for approved in [false, true] {
            let (tv, waiting, code) = tv_asks(&server, true).await;
            assert!(
                alice
                    .device_sign_in_lookup(code.clone())
                    .await
                    .unwrap()
                    .remember
            );
            alice.device_sign_in_approve(code, approved).await.unwrap();
            match waiting.await.unwrap() {
                Response::Ok(Reply::LoggedIn { remembered, .. }) => {
                    assert_eq!(remembered, approved);
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(trusted_names(&tv).await.len(), usize::from(approved));
            tv.close().await;
        }
        server.stop().await;
    }
}
