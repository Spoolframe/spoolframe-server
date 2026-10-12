//! A user leaves a server from an app (PX-47): a portable user's grant
//! and data go; a local account is set aside for an admin to restore; the
//! last admin stays; and leaving needs a recent sign-in.

use std::time::Duration;

use spoolframe_protocol::{Error, Reply, Request, Response, SessionId, WhoAmIReply};
use spoolframe_server::auth::hash_password;
use spoolframe_server::principal::Principal;
use spoolframe_server::store::watch::WatchUpdate;
use spoolframe_types::{Role, TrackPreferences};

use crate::portable_sign_in::{identity, session, sign_in};
use crate::support::{TestClient, TestServer};

const PASSWORD: &str = "correct horse";
const DONE: Response = Response::Ok(Reply::Done);

async fn watched(server: &TestServer, principal: Principal) {
    server
        .store
        .save_watch(
            principal,
            WatchUpdate {
                media_id: "plex:movie:1",
                show_id: None,
                external_ids: &[],
                position_ms: Some(60_000),
                duration_ms: Some(6_000_000),
                watched: false,
                unhide: false,
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_portable_user_who_leaves_loses_its_grant_and_its_data_here() {
    let server = TestServer::start("leave-portable").await;
    let sam = identity(7);
    server
        .store
        .grant_portable(sam, Some("Sam"), None)
        .await
        .unwrap();
    let tv = server.client().await;
    tv.hello_as(Some("Living room")).await;
    let token = session(sign_in(&tv, 7, 9).await);
    let principal = Principal::PortableUser(sam);
    watched(&server, principal).await;
    server
        .store
        .hide_continue(principal, "plex:movie:2", &[])
        .await
        .unwrap();
    server
        .store
        .set_track_preferences(principal, &TrackPreferences::default())
        .await
        .unwrap();

    assert_eq!(tv.call(Some(&token), Request::LeaveServer).await, DONE);

    let grant = server.store.grant_of(sam).await.unwrap();
    assert!(grant.is_none(), "{grant:?}");
    assert!(server.store.all_watch(principal).await.unwrap().is_empty());
    assert!(server.store.hidden(principal).await.unwrap().is_empty());
    // The session ended with the grant, and the server closed the
    // connection; signing in again, from anywhere, is refused.
    let again = server.client().await;
    again.hello_as(Some("Living room")).await;
    assert!(matches!(sign_in(&again, 7, 9).await, Response::Err(_)));
}

#[tokio::test]
async fn a_local_user_who_leaves_is_set_aside_and_an_admin_restores_them() {
    let server = TestServer::start("leave-local").await;
    server.add_user("alice", PASSWORD, Some("Alice")).await;
    let phone = server.client().await;
    phone.hello_as(Some("Alice's phone")).await;
    let token = phone.login("alice", PASSWORD).await;
    let alice = server
        .store
        .user_by_name("alice")
        .await
        .unwrap()
        .unwrap()
        .id;
    watched(&server, Principal::LocalUser(alice)).await;

    assert_eq!(phone.call(Some(&token), Request::LeaveServer).await, DONE);

    let record = server.store.user_by_name("alice").await.unwrap().unwrap();
    assert!(record.left_at.is_some());
    assert!(record.disabled_at.is_some());
    assert_eq!(record.session_count, 0);
    // Their data stays.
    assert_eq!(
        server
            .store
            .all_watch(Principal::LocalUser(alice))
            .await
            .unwrap()
            .len(),
        1
    );
    let other = server.client().await;
    other.hello_as(Some("Another phone")).await;
    assert!(matches!(
        other
            .call(
                None,
                Request::LoginPassword(spoolframe_protocol::LoginPassword {
                    username: "alice".into(),
                    password: PASSWORD.into(),
                    remember: false,
                }),
            )
            .await,
        Response::Err(_)
    ));

    // An admin restores them as they enable one.
    server.store.enable_user("alice").await.unwrap();
    let record = server.store.user_by_name("alice").await.unwrap().unwrap();
    assert_eq!((record.left_at, record.disabled_at), (None, None));
    other.login("alice", PASSWORD).await;
}

#[tokio::test]
async fn the_last_admin_cannot_leave_and_another_admin_can() {
    let server = TestServer::start("leave-admin").await;
    let phc = hash_password(PASSWORD).await.unwrap();
    server
        .store
        .add_user("root", None, &phc, Role::Admin)
        .await
        .unwrap();
    let phone = server.client().await;
    phone.hello_as(Some("Root's phone")).await;
    let token = phone.login("root", PASSWORD).await;
    assert_eq!(
        phone.call(Some(&token), Request::LeaveServer).await,
        Response::Err(Error::LastAdmin)
    );
    let record = server.store.user_by_name("root").await.unwrap().unwrap();
    assert_eq!((record.left_at, record.disabled_at), (None, None));

    server
        .store
        .add_user("second", None, &phc, Role::Admin)
        .await
        .unwrap();
    assert_eq!(phone.call(Some(&token), Request::LeaveServer).await, DONE);
}

#[tokio::test]
async fn leaving_needs_a_recent_sign_in_and_a_portable_one_signs_in_again() {
    let server = TestServer::start("leave-recent").await;
    server.add_user("alice", PASSWORD, Some("Alice")).await;
    let sam = identity(7);
    server
        .store
        .grant_portable(sam, Some("Sam"), None)
        .await
        .unwrap();
    let phone = server.client().await;
    phone.hello_as(Some("Alice's phone")).await;
    let alice = phone.login("alice", PASSWORD).await;
    let tv = server.client().await;
    tv.hello_as(Some("Living room")).await;
    let old = session(sign_in(&tv, 7, 9).await);

    server.clock.advance(Duration::from_secs(11 * 60));
    let not_recent = Response::Err(Error::RecentSignInRequired);
    assert_eq!(
        phone.call(Some(&alice), Request::LeaveServer).await,
        not_recent
    );
    assert_eq!(tv.call(Some(&old), Request::LeaveServer).await, not_recent);
    let record = server.store.user_by_name("alice").await.unwrap().unwrap();
    assert_eq!(record.left_at, None);

    // A portable user's credential, presented again, is its recent sign-in.
    let fresh = session(sign_in(&tv, 7, 9).await);
    assert_eq!(tv.call(Some(&fresh), Request::LeaveServer).await, DONE);
}

async fn can_leave(client: &TestClient, token: &SessionId) -> bool {
    match client.call(Some(token), Request::WhoAmI).await {
        Response::Ok(Reply::WhoAmI(WhoAmIReply { can_leave, .. })) => can_leave,
        other => panic!("not WhoAmI: {other:?}"),
    }
}

/// `WhoAmI` says ahead of time what `LeaveServer` would: the last admin
/// cannot leave, until there is another; anyone else can.
#[tokio::test]
async fn who_am_i_says_whether_leaving_would_be_refused() {
    let server = TestServer::start("leave-who-am-i").await;
    let phc = hash_password(PASSWORD).await.unwrap();
    server
        .store
        .add_user("root", None, &phc, Role::Admin)
        .await
        .unwrap();
    server.add_user("alice", PASSWORD, Some("Alice")).await;
    server
        .store
        .grant_portable(identity(7), Some("Sam"), None)
        .await
        .unwrap();
    let phone = server.client().await;
    phone.hello_as(Some("Root's phone")).await;
    let root = phone.login("root", PASSWORD).await;
    let alice = phone.login("alice", PASSWORD).await;
    let tv = server.client().await;
    tv.hello_as(Some("Living room")).await;
    let sam = session(sign_in(&tv, 7, 9).await);

    assert!(!can_leave(&phone, &root).await, "the only admin");
    assert!(can_leave(&phone, &alice).await, "a consumer");
    assert!(can_leave(&tv, &sam).await, "a portable user");

    server
        .store
        .add_user("second", None, &phc, Role::Admin)
        .await
        .unwrap();
    assert!(can_leave(&phone, &root).await, "one of two admins");
}
