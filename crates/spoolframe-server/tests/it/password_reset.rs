//! An admin resets a password to a one-time one: it ends what rested on
//! the old password, and signs in once to a session that may only choose
//! a new one, on a device and on the web.

use std::net::SocketAddr;
use std::time::Duration;

use spoolframe_admin_api::{
    AdminApiClient, MeApiClient, OneTimePassword, PasswordChange, error_code,
};
use spoolframe_protocol::{
    ChangePassword, Error, LoginPassword, Reply, Request, Response, SessionId,
};
use spoolframe_server::auth::{SessionToken, hash_password};
use spoolframe_server::store::sessions::{SessionBinding, SignedInWith};
use spoolframe_types::{Platform, Role};

use crate::admin::{anonymous, code, creds, serve_beside, with_token};
use crate::support::{TestClient, TestServer};

const WEB: &str = "http://localhost:8080";

/// A server with root (an admin) and alice, the admin API beside it, and
/// root signed in there.
async fn server(test: &str) -> (TestServer, SocketAddr, jsonrpsee::ws_client::WsClient) {
    let server = TestServer::start(test).await;
    let phc = hash_password("root password").await.unwrap();
    server
        .store
        .add_user("root", None, &phc, Role::Admin)
        .await
        .unwrap();
    server.add_user("alice", "alice's password", None).await;
    let web = serve_beside(&server, WEB).await;
    let root = anonymous(web)
        .await
        .login(creds("root", "root password"))
        .await
        .unwrap();
    let root = with_token(web, &root.token).await;
    (server, web, root)
}

async fn reset(root: &jsonrpsee::ws_client::WsClient, username: &str) -> OneTimePassword {
    root.user_reset_password(username.into()).await.unwrap()
}

async fn login(client: &TestClient, username: &str, password: &str) -> Response {
    client
        .call(
            None,
            Request::LoginPassword(LoginPassword {
                username: username.into(),
                password: password.into(),
                remember: false,
            }),
        )
        .await
}

/// A live session of `user` on `client`'s device, signed in as `with`
/// says, made in the store as a sign-in of that kind would.
async fn session_on(
    server: &TestServer,
    client: &TestClient,
    username: &str,
    with: Option<SignedInWith>,
) -> SessionId {
    let user = server
        .store
        .list_users()
        .await
        .unwrap()
        .into_iter()
        .find(|u| u.username == username)
        .unwrap()
        .id;
    let token = format!("{username}-{with:?}");
    server
        .store
        .create_session(
            &SessionToken(token.clone()).hash(),
            user,
            &SessionBinding::Endpoint {
                endpoint_id: client.endpoint.id().to_string(),
                device_name: None,
                platform: Platform::Linux,
            },
            i64::MAX / 2,
            with,
        )
        .await
        .unwrap();
    SessionId(token)
}

fn alive(response: &Response) -> bool {
    matches!(response, Response::Ok(Reply::WhoAmI(_)))
}

#[tokio::test]
async fn a_reset_ends_what_rested_on_the_password_and_opens_one_session_to_change_it() {
    let (server, _web, root) = server("reset-password").await;
    let tv = server.client().await;
    tv.hello().await;
    let by_password = tv.login("alice", "alice's password").await;
    let phone = server.client().await;
    phone.hello().await;
    let by_passkey = session_on(&server, &phone, "alice", Some(SignedInWith::Passkey)).await;
    let unknown = session_on(&server, &phone, "alice", None).await;

    let one_time = reset(&root, "alice").await;
    assert_eq!(one_time.password.len(), 19, "{one_time:?}");
    assert!(one_time.revoked_sessions >= 2, "{one_time:?}");

    // What rested on the old password, or might have, has ended. The
    // server closes the connections of sessions that end: each device
    // connects again, as an app does.
    let tv = tv.reconnect(&server).await;
    let phone = phone.reconnect(&server).await;
    for (name, device, session, kept) in [
        ("password", &tv, &by_password, false),
        ("unknown origin", &phone, &unknown, false),
        ("passkey", &phone, &by_passkey, true),
    ] {
        let who = device.call(Some(session), Request::WhoAmI).await;
        assert_eq!(alive(&who), kept, "{name}: {who:?}");
    }
    assert_eq!(
        login(&tv, "alice", "alice's password").await,
        Response::Err(Error::InvalidCredentials)
    );

    // The one-time password opens a session that may only change it.
    let Response::Ok(Reply::LoggedIn {
        session,
        remembered: false,
        password_change_required: true,
        ..
    }) = login(&tv, "alice", &one_time.password).await
    else {
        panic!("the one-time password did not sign in");
    };
    assert!(alive(&tv.call(Some(&session), Request::WhoAmI).await));
    assert_eq!(
        tv.call(Some(&session), Request::Libraries).await,
        Response::Err(Error::PasswordChangeRequired)
    );
    let change = |current: &str, new: &str| {
        Request::ChangePassword(ChangePassword {
            current_password: current.into(),
            new_password: new.into(),
        })
    };
    assert!(matches!(
        tv.call(
            Some(&session),
            change(&one_time.password, &one_time.password)
        )
        .await,
        Response::Err(Error::InvalidInput { .. })
    ));
    assert_eq!(
        tv.call(
            Some(&session),
            change(&one_time.password, "alice's new password")
        )
        .await,
        Response::Ok(Reply::Done)
    );
    // The session may do anything now; the passkey one was ended with the
    // others by the change, as any password change ends them.
    assert_ne!(
        tv.call(Some(&session), Request::Libraries).await,
        Response::Err(Error::PasswordChangeRequired)
    );
    assert_eq!(
        login(&tv, "alice", &one_time.password).await,
        Response::Err(Error::InvalidCredentials)
    );
    assert!(matches!(
        login(&tv, "alice", "alice's new password").await,
        Response::Ok(Reply::LoggedIn {
            password_change_required: false,
            ..
        })
    ));
    tv.close().await;
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_one_time_password_stops_working_after_a_week() {
    let (server, _web, root) = server("reset-password-expiry").await;
    let one_time = reset(&root, "alice").await;
    server.clock.advance(Duration::from_secs(7 * 24 * 3600 + 1));
    let tv = server.client().await;
    tv.hello().await;
    assert_eq!(
        login(&tv, "alice", &one_time.password).await,
        Response::Err(Error::InvalidCredentials)
    );
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn only_an_admin_resets_and_not_their_own() {
    let (server, web, root) = server("reset-password-who").await;
    let alice = anonymous(web)
        .await
        .login(creds("alice", "alice's password"))
        .await
        .unwrap();
    let alice = with_token(web, &alice.token).await;
    assert_eq!(
        code(alice.user_reset_password("root".into()).await.unwrap_err()),
        error_code::FORBIDDEN
    );
    assert_eq!(
        code(root.user_reset_password("root".into()).await.unwrap_err()),
        error_code::INVALID_INPUT
    );
    server.stop().await;
}

/// On the web: the sign-in says to choose a new password, every call but
/// `whoAmI`, `logout` and `me.setPassword` is refused until then, and the
/// change lifts it.
#[tokio::test]
async fn the_web_signs_in_with_a_one_time_password_only_to_change_it() {
    let (server, web, root) = server("reset-password-web").await;
    let one_time = reset(&root, "alice").await;
    let signed_in = anonymous(web)
        .await
        .login(creds("alice", &one_time.password))
        .await
        .unwrap();
    assert!(signed_in.password_change_required);
    let alice = with_token(web, &signed_in.token).await;
    let me = alice.who_am_i().await.unwrap();
    assert!(me.password_must_change, "{me:?}");
    assert_eq!(
        code(alice.sessions().await.unwrap_err()),
        error_code::PASSWORD_CHANGE_REQUIRED
    );
    alice
        .set_password(PasswordChange {
            current_password: one_time.password.clone(),
            new_password: "alice's new password".into(),
        })
        .await
        .unwrap();
    alice.sessions().await.unwrap();
    assert!(!alice.who_am_i().await.unwrap().password_must_change);
    server.stop().await;
}

/// The browser's own sign-in (`/auth/login`) says so too: what the web
/// UI reads to open "Choose a new password".
#[tokio::test]
async fn a_browser_sign_in_says_a_new_password_is_required() {
    let (server, web, root) = server("reset-password-browser").await;
    let one_time = reset(&root, "alice").await;
    let signed_in = |username: &'static str, password: String| async move {
        let response = reqwest::Client::new()
            .post(format!("http://{web}/auth/login"))
            .header("origin", WEB)
            .json(&serde_json::json!({ "username": username, "password": password }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{response:?}");
        let body: serde_json::Value = response.json().await.unwrap();
        body["passwordChangeRequired"].as_bool().unwrap()
    };
    assert!(signed_in("alice", one_time.password.clone()).await);
    assert!(!signed_in("root", "root password".into()).await);
    server.stop().await;
}

/// A call in a one-time password's session is refused until the password
/// is changed, and the SDK says so on `profile_events` too.
#[tokio::test]
async fn the_sdk_tells_a_password_to_change_as_a_profile_event() {
    use spoolframe_client_sdk::{MemoryStorage, ProfileChange, SdkError};
    let (server, _web, root) = server("one-time-profile-event").await;
    let once = reset(&root, "alice").await;
    let client = crate::support::sdk_client(
        std::sync::Arc::new(MemoryStorage::default()),
        spoolframe_client_sdk::Device {
            name: "phone".into(),
            client_name: "sdk-test".into(),
            client_version: "0".into(),
            platform: Platform::Ios,
            capabilities: Default::default(),
        },
    )
    .await
    .unwrap();
    let id = server.server.id().to_string();
    let addrs = server.server.addr().ip_addrs().copied().collect();
    client.add_address(&id, addrs).unwrap();
    client.connect(&id).await.unwrap();
    client.pin(&id, true).await.unwrap();
    let session = client
        .sign_in(&id, "alice", &once.password, false)
        .await
        .unwrap();
    let mut events = client.profile_events();

    assert_eq!(
        session.libraries().await.unwrap_err(),
        SdkError::PasswordChangeRequired
    );
    let told = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("no profile event within 5 s")
        .unwrap();
    assert_eq!(told.server, id);
    assert_eq!(
        told.change,
        ProfileChange::PasswordChangeRequired {
            user: session.user().user_id.clone()
        }
    );
    client.close().await;
    server.stop().await;
}
