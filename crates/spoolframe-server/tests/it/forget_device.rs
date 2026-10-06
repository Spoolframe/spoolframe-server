//! Deleting a device (P4-03): a user's sessions on one of their devices
//! are deleted, with its trust and PIN requirement, so it leaves their
//! devices; another user's rows there are kept. From an app
//! (`ForgetDevice`), the web UI (`me.forgetDevice`) and an admin's user
//! page (`admin.userForgetDevice`).

use crate::support;

use std::time::Duration;

use iroh::endpoint::{Connection, ConnectionError};
use spoolframe_admin_api::{AdminApiClient, MeApiClient, error_code};
use spoolframe_protocol::{
    ConfirmPassword, DeviceInfo, Error, LoginPassword, Reply, Request, Response, SessionId, close,
};
use support::{ServerOptions, TestClient, TestServer, fresh_dir};

const PASSWORD: &str = "correct horse";
const BOB: &str = "battery staple";
const PIN: &str = "4321";
const WEB: &str = "https://media.example.org";

/// How long the server is given to close a connection it is to close.
const AT_ONCE: Duration = Duration::from_secs(5);
/// How long a connection that is to stay open is watched.
const A_WHILE: Duration = Duration::from_millis(1500);

async fn server(test: &str) -> TestServer {
    let server = TestServer::start_with(fresh_dir(test).await, ServerOptions::default()).await;
    server.add_user("alice", PASSWORD, Some("Alice")).await;
    server.add_user("bob", BOB, None).await;
    server
}

/// A device that said Hello.
async fn device(server: &TestServer, name: &str) -> TestClient {
    let client = server.client().await;
    client.hello_as(Some(name)).await;
    client
}

/// `username` signed in on `client`, remembering it: the device is trusted
/// for them.
async fn remembered(client: &TestClient, username: &str, password: &str) -> SessionId {
    let request = Request::LoginPassword(LoginPassword {
        username: username.into(),
        password: password.into(),
        remember: true,
    });
    match client.call(None, request).await {
        Response::Ok(Reply::LoggedIn {
            session,
            remembered,
        }) => {
            assert!(remembered);
            session
        }
        other => panic!("sign-in as {username} failed: {other:?}"),
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

fn forget(device: &TestClient) -> Request {
    Request::ForgetDevice {
        device: device.endpoint.id().to_string(),
    }
}

async fn devices(client: &TestClient, session: &SessionId) -> Vec<DeviceInfo> {
    match ok(client, session, Request::ListDevices).await {
        Reply::Devices { devices } => devices,
        other => panic!("{other:?}"),
    }
}

/// Whether `client`'s device is in `session`'s user's devices.
async fn listed(by: &TestClient, session: &SessionId, client: &TestClient) -> bool {
    let id = client.endpoint.id().to_string();
    devices(by, session)
        .await
        .iter()
        .any(|d| d.endpoint_id == id)
}

/// Who `client`'s device opens without a password.
async fn trusted_names(client: &TestClient) -> Vec<String> {
    match client.call(None, Request::TrustedProfiles).await {
        Response::Ok(Reply::TrustedProfiles { profiles }) => {
            profiles.into_iter().map(|p| p.username).collect()
        }
        other => panic!("{other:?}"),
    }
}

/// The code the server closed `conn` with, if it did within `wait`.
async fn closed_with(conn: &Connection, wait: Duration) -> Option<u32> {
    match tokio::time::timeout(wait, conn.closed()).await {
        Ok(ConnectionError::ApplicationClosed(close)) => Some(
            close
                .error_code
                .into_inner()
                .try_into()
                .expect("a u32 code"),
        ),
        Ok(other) => panic!("closed otherwise: {other:?}"),
        Err(_) => None,
    }
}

/// Every session as if signed in an hour ago: past the recent sign-in.
async fn age_sign_ins(server: &TestServer) {
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(server.dir.join("spoolframe.db")),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE sessions SET authenticated_at = authenticated_at - 3600")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

async fn confirm(client: &TestClient, session: &SessionId) {
    ok(
        client,
        session,
        Request::ConfirmPassword(ConfirmPassword {
            password: PASSWORD.into(),
        }),
    )
    .await;
}

/// Alice on a TV that asks for her PIN and is trusted for her, and on a
/// laptop, from which she deletes the TV.
async fn alice_on_a_tv_and_a_laptop(
    server: &TestServer,
) -> (TestClient, SessionId, TestClient, SessionId) {
    let tv = device(server, "Living room TV").await;
    let on_tv = remembered(&tv, "alice", PASSWORD).await;
    ok(
        &tv,
        &on_tv,
        Request::SetPin {
            pin: PIN.into(),
            proof: None,
        },
    )
    .await;
    ok(
        &tv,
        &on_tv,
        Request::SetPinRequired {
            device: None,
            required: true,
        },
    )
    .await;
    let laptop = device(server, "Laptop").await;
    let on_laptop = remembered(&laptop, "alice", PASSWORD).await;
    (tv, on_tv, laptop, on_laptop)
}

/// An app deletes another device: it leaves the list, its sessions are
/// deleted, its trust ends, and its connection closes as signed out.
#[tokio::test]
async fn a_deleted_device_leaves_the_list_and_is_signed_out() {
    let server = server("forget-app").await;
    let (tv, on_tv, laptop, on_laptop) = alice_on_a_tv_and_a_laptop(&server).await;
    assert!(listed(&laptop, &on_laptop, &tv).await);

    assert_eq!(ok(&laptop, &on_laptop, forget(&tv)).await, Reply::Done);

    assert!(!listed(&laptop, &on_laptop, &tv).await);
    assert_eq!(
        closed_with(&tv.conn, AT_ONCE).await,
        Some(close::SIGNED_OUT)
    );
    let tv = tv.reconnect(&server).await;
    tv.hello().await;
    assert!(trusted_names(&tv).await.is_empty());
    assert_eq!(
        err(&tv, &on_tv, Request::WhoAmI).await,
        Error::Unauthenticated
    );
    // Signed in again, it is a new device: no PIN asked for, not trusted.
    let again = match tv
        .call(
            None,
            Request::LoginPassword(LoginPassword {
                username: "alice".into(),
                password: PASSWORD.into(),
                remember: false,
            }),
        )
        .await
    {
        Response::Ok(Reply::LoggedIn { session, .. }) => session,
        other => panic!("{other:?}"),
    };
    let id = tv.endpoint.id().to_string();
    let listed = devices(&tv, &again).await;
    let it = listed.iter().find(|d| d.endpoint_id == id).unwrap();
    assert!(!it.pin_required && !it.trusted, "{it:?}");
    server.stop().await;
}

/// Its own device an app signs out of with `Logout`; deleting it is
/// refused, recent sign-in or not, and nothing changes.
#[tokio::test]
async fn an_app_cannot_delete_its_own_device() {
    let server = server("forget-own").await;
    let tv = device(&server, "Living room TV").await;
    let on_tv = remembered(&tv, "alice", PASSWORD).await;
    assert!(matches!(
        err(&tv, &on_tv, forget(&tv)).await,
        Error::InvalidInput { .. }
    ));
    assert!(listed(&tv, &on_tv, &tv).await);
    assert_eq!(trusted_names(&tv).await, ["alice"]);
    server.stop().await;
}

/// A session left signed in deletes nothing; confirmed, it does.
#[tokio::test]
async fn deleting_a_device_needs_a_recent_sign_in() {
    let server = server("forget-recent").await;
    let (tv, _, laptop, on_laptop) = alice_on_a_tv_and_a_laptop(&server).await;
    age_sign_ins(&server).await;
    assert_eq!(
        err(&laptop, &on_laptop, forget(&tv)).await,
        Error::RecentSignInRequired
    );
    assert!(listed(&laptop, &on_laptop, &tv).await);
    assert_eq!(trusted_names(&tv).await, ["alice"]);

    confirm(&laptop, &on_laptop).await;
    ok(&laptop, &on_laptop, forget(&tv)).await;
    assert!(!listed(&laptop, &on_laptop, &tv).await);
    server.stop().await;
}

/// A device the user never signed in on is not theirs to delete.
#[tokio::test]
async fn an_unknown_device_is_not_found() {
    let server = server("forget-unknown").await;
    let laptop = device(&server, "Laptop").await;
    let on_laptop = remembered(&laptop, "alice", PASSWORD).await;
    let stranger = device(&server, "Stranger").await;
    assert!(matches!(
        err(&laptop, &on_laptop, forget(&stranger)).await,
        Error::NotFound { .. }
    ));
    server.stop().await;
}

/// A profile locked by its PIN deletes nothing.
#[tokio::test]
async fn a_locked_profile_cannot_delete_a_device() {
    let server = server("forget-locked").await;
    let (tv, _, laptop, on_laptop) = alice_on_a_tv_and_a_laptop(&server).await;
    ok(
        &laptop,
        &on_laptop,
        Request::SetPinRequired {
            device: None,
            required: true,
        },
    )
    .await;
    let laptop = laptop.reconnect(&server).await;
    laptop.hello().await;
    assert_eq!(
        err(&laptop, &on_laptop, forget(&tv)).await,
        Error::ProfileLocked
    );
    assert_eq!(trusted_names(&tv).await, ["alice"]);
    server.stop().await;
}

/// Bob shares the TV: Alice deleting it leaves his session, trust and PIN
/// requirement there, and its connection open.
#[tokio::test]
async fn another_user_s_rows_on_the_device_are_kept() {
    let server = server("forget-shared").await;
    let (tv, _, laptop, on_laptop) = alice_on_a_tv_and_a_laptop(&server).await;
    let bob_on_tv = remembered(&tv, "bob", BOB).await;
    ok(
        &tv,
        &bob_on_tv,
        Request::SetPin {
            pin: PIN.into(),
            proof: None,
        },
    )
    .await;
    ok(
        &tv,
        &bob_on_tv,
        Request::SetPinRequired {
            device: None,
            required: true,
        },
    )
    .await;

    ok(&laptop, &on_laptop, forget(&tv)).await;

    assert_eq!(closed_with(&tv.conn, A_WHILE).await, None);
    assert_eq!(trusted_names(&tv).await, ["bob"]);
    let id = tv.endpoint.id().to_string();
    let his = devices(&tv, &bob_on_tv).await;
    let it = his.iter().find(|d| d.endpoint_id == id).unwrap();
    assert!(it.trusted && it.pin_required, "{it:?}");
    server.stop().await;
}

mod web {
    use super::*;

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

    fn id(client: &TestClient) -> String {
        client.endpoint.id().to_string()
    }

    /// `me.forgetDevice`: the browser's session is on no device, so any
    /// of the user's may be deleted, after a recent sign-in.
    #[tokio::test]
    async fn a_user_deletes_a_device_from_the_web_after_a_recent_sign_in() {
        let server = server("forget-web-me").await;
        let web = crate::admin::serve_beside(&server, WEB).await;
        let (tv, _, _laptop, _) = alice_on_a_tv_and_a_laptop(&server).await;
        let alice = on_the_web(web, "alice", PASSWORD).await;

        let stranger = device(&server, "Stranger").await;
        assert_eq!(
            crate::admin::code(alice.forget_device(id(&stranger)).await.unwrap_err()),
            error_code::NO_SUCH_DEVICE
        );
        age_sign_ins(&server).await;
        assert_eq!(
            crate::admin::code(alice.forget_device(id(&tv)).await.unwrap_err()),
            error_code::RECENT_SIGN_IN_REQUIRED
        );
        assert!(
            alice
                .devices()
                .await
                .unwrap()
                .iter()
                .any(|d| d.endpoint_id == id(&tv))
        );

        alice.confirm_password(PASSWORD.into()).await.unwrap();
        alice.forget_device(id(&tv)).await.unwrap();
        assert!(
            alice
                .devices()
                .await
                .unwrap()
                .iter()
                .all(|d| d.endpoint_id != id(&tv))
        );
        assert!(
            alice
                .sessions()
                .await
                .unwrap()
                .iter()
                .all(|s| s.endpoint_id.as_deref() != Some(id(&tv).as_str()))
        );
        assert_eq!(
            closed_with(&tv.conn, AT_ONCE).await,
            Some(close::SIGNED_OUT)
        );
        server.stop().await;
    }

    /// `admin.userForgetDevice`: an admin's, not a consumer's, and only
    /// the named user's rows go.
    #[tokio::test]
    async fn an_admin_deletes_a_user_s_device_and_keeps_the_others_rows() {
        let server = server("forget-web-admin").await;
        let web = crate::admin::serve_beside(&server, WEB).await;
        let (tv, _, _laptop, _) = alice_on_a_tv_and_a_laptop(&server).await;
        let bob_on_tv = remembered(&tv, "bob", BOB).await;
        let alice = on_the_web(web, "alice", PASSWORD).await;
        assert_eq!(
            crate::admin::code(
                alice
                    .user_forget_device("bob".into(), id(&tv))
                    .await
                    .unwrap_err()
            ),
            error_code::FORBIDDEN
        );
        server
            .store
            .set_role("bob", spoolframe_types::Role::Admin)
            .await
            .unwrap();
        let root = on_the_web(web, "bob", BOB).await;
        assert_eq!(
            crate::admin::code(
                root.user_forget_device("alice".into(), "ab".repeat(32))
                    .await
                    .unwrap_err()
            ),
            error_code::NO_SUCH_DEVICE
        );

        root.user_forget_device("alice".into(), id(&tv))
            .await
            .unwrap();

        assert!(
            root.user_devices("alice".into())
                .await
                .unwrap()
                .iter()
                .all(|d| d.endpoint_id != id(&tv))
        );
        assert!(
            root.user_devices("bob".into())
                .await
                .unwrap()
                .iter()
                .any(|d| d.endpoint_id == id(&tv) && d.trusted)
        );
        assert_eq!(trusted_names(&tv).await, ["bob"]);
        assert!(matches!(
            ok(&tv, &bob_on_tv, Request::WhoAmI).await,
            Reply::WhoAmI(_)
        ));
        server.stop().await;
    }
}
