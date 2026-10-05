//! Sessions bound to the endpoint that logged in, several per connection
//! (P1-12).

use crate::support;

use spoolframe_protocol::{Error, PlaybackPlan, Reply, Request, Response, UserSummary};
use spoolframe_types::{ClientCapabilities, Platform};
use support::{TestServer, movie};

async fn who(client: &support::TestClient, session: &spoolframe_protocol::SessionId) -> Response {
    client.call(Some(session), Request::WhoAmI).await
}

fn name_of(response: Response) -> String {
    match response {
        Response::Ok(Reply::WhoAmI(UserSummary { display_name, .. })) => display_name,
        other => panic!("not WhoAmI: {other:?}"),
    }
}

#[tokio::test]
async fn a_session_works_only_from_the_endpoint_that_logged_in() {
    let server = TestServer::start("own-endpoint").await;
    server.add_user("alice", "correct horse", None).await;
    let tv = server.client().await;
    let thief = server.client().await;
    tv.hello().await;
    thief.hello().await;
    let session = tv.login("alice", "correct horse").await;

    assert_eq!(name_of(who(&tv, &session).await), "alice");
    assert_eq!(
        who(&thief, &session).await,
        Response::Err(Error::Unauthenticated)
    );
    tv.close().await;
    thief.close().await;
    server.stop().await;
}

/// P3-20: `local:` and a lowercase, hyphenated UUIDv7, not a number.
#[tokio::test]
async fn who_am_i_names_the_user_by_a_uuidv7() {
    let server = TestServer::start("uuid-user-id").await;
    server.add_user("alice", "correct horse", None).await;
    let tv = server.client().await;
    tv.hello().await;
    let session = tv.login("alice", "correct horse").await;
    let Response::Ok(Reply::WhoAmI(UserSummary { user_id, .. })) = who(&tv, &session).await else {
        panic!("not WhoAmI");
    };
    let uuid = user_id.strip_prefix("local:").expect(&user_id);
    let groups: Vec<_> = uuid.split('-').map(str::len).collect();
    assert_eq!(groups, [8, 4, 4, 4, 12], "{user_id}");
    assert!(
        uuid.chars()
            .all(|c| c == '-' || c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "{user_id}"
    );
    assert_eq!(&uuid[14..15], "7", "version 7: {user_id}");
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn two_users_share_one_connection() {
    let server = TestServer::start("two-users").await;
    server
        .add_user("alice", "correct horse", Some("Alice"))
        .await;
    server.add_user("bob", "battery staple", Some("Bob")).await;
    let tv = server.client().await;
    tv.hello().await;
    let alice = tv.login("alice", "correct horse").await;
    let bob = tv.login("bob", "battery staple").await;

    // Interleaved on concurrent streams of the same connection.
    let (a, b, a2, b2) = tokio::join!(
        who(&tv, &alice),
        who(&tv, &bob),
        who(&tv, &alice),
        who(&tv, &bob)
    );
    assert_eq!(
        [name_of(a), name_of(b), name_of(a2), name_of(b2)],
        ["Alice", "Bob", "Alice", "Bob"]
    );
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn logout_ends_only_the_calling_session() {
    let server = TestServer::start("logout").await;
    server.add_user("alice", "correct horse", None).await;
    server.add_user("bob", "battery staple", None).await;
    let tv = server.client().await;
    tv.hello().await;
    let alice = tv.login("alice", "correct horse").await;
    let bob = tv.login("bob", "battery staple").await;

    assert_eq!(
        tv.call(Some(&alice), Request::Logout).await,
        Response::Ok(Reply::LoggedOut)
    );
    assert_eq!(
        who(&tv, &alice).await,
        Response::Err(Error::Unauthenticated)
    );
    assert_eq!(name_of(who(&tv, &bob).await), "bob");
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_disabled_user_s_session_stops_working() {
    let server = TestServer::start("disabled").await;
    server.add_user("alice", "correct horse", None).await;
    let tv = server.client().await;
    tv.hello().await;
    let alice = tv.login("alice", "correct horse").await;
    server.store.disable_user("alice").await.unwrap();
    assert_eq!(
        who(&tv, &alice).await,
        Response::Err(Error::Unauthenticated)
    );
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn sessions_are_listed_with_device_names_and_no_tokens() {
    let server = TestServer::start("list").await;
    server.add_user("alice", "correct horse", None).await;
    let tv = server.client().await;
    let laptop = server.client().await;
    tv.hello_as(Some("Living room")).await;
    laptop.hello_as(Some("Laptop")).await;
    let on_tv = tv.login("alice", "correct horse").await;
    laptop.login("alice", "correct horse").await;

    let Response::Ok(Reply::Sessions { sessions }) =
        tv.call(Some(&on_tv), Request::ListSessions).await
    else {
        panic!("no sessions");
    };
    let mut devices: Vec<_> = sessions
        .iter()
        .map(|s| (s.device_name.clone().unwrap_or_default(), s.current))
        .collect();
    devices.sort();
    assert_eq!(
        devices,
        [
            ("Laptop".to_owned(), false),
            ("Living room".to_owned(), true)
        ]
    );
    let shown = format!("{sessions:?}");
    assert!(!shown.contains(&on_tv.0), "a token leaked: {shown}");
    tv.close().await;
    laptop.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_session_survives_a_server_restart() {
    let server = TestServer::start("restart").await;
    server.add_user("alice", "correct horse", None).await;
    let dir = server.dir.clone();
    // The client keeps its endpoint (its identity) across the restart.
    let tv = server.client().await;
    tv.hello().await;
    let alice = tv.login("alice", "correct horse").await;
    tv.conn.close(0u32.into(), b"restart");
    server.stop().await;

    let server = TestServer::start_in(dir).await;
    let conn = tv
        .endpoint
        .connect(server.server.addr(), spoolframe_protocol::CLIENT_ALPN)
        .await
        .unwrap();
    let tv = support::TestClient {
        endpoint: tv.endpoint,
        conn,
    };
    tv.hello().await;
    assert_eq!(name_of(who(&tv, &alice).await), "alice");
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn the_catalog_is_served_in_a_session() {
    let server = TestServer::start("catalog").await;
    server.add_user("alice", "correct horse", None).await;
    let tv = server.client().await;
    tv.hello().await;
    let alice = tv.login("alice", "correct horse").await;

    let Response::Ok(Reply::Libraries { libraries }) =
        tv.call(Some(&alice), Request::Libraries).await
    else {
        panic!("no libraries");
    };
    assert_eq!(libraries, [support::library()]);
    let id = movie().id().clone();
    let Response::Ok(Reply::Item { item, .. }) = tv.call(Some(&alice), Request::Item { id }).await
    else {
        panic!("no item");
    };
    assert_eq!(item, movie());
    let missing = spoolframe_types::MediaId::new(
        spoolframe_types::Namespace::Plex,
        spoolframe_types::MediaKind::Movie,
        "404",
    )
    .unwrap();
    assert!(matches!(
        tv.call(Some(&alice), Request::Item { id: missing }).await,
        Response::Err(Error::NotFound { .. })
    ));
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_client_without_capabilities_gets_an_unsupported_plan() {
    let server = TestServer::start("plan-no-caps").await;
    server.add_user("alice", "correct horse", None).await;
    let tv = server.client().await;
    tv.hello().await;
    let alice = tv.login("alice", "correct horse").await;
    let request = Request::PlaybackPlan {
        id: movie().id().clone(),
        selection: Default::default(),
        bitmap: Default::default(),
    };
    let Response::Ok(Reply::PlaybackPlan {
        plan: PlaybackPlan::Unsupported { reasons },
        ..
    }) = tv.call(Some(&alice), request).await
    else {
        panic!("not an unsupported plan");
    };
    assert_eq!(reasons, ["the client sent no capabilities"]);
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_plan_for_an_item_without_a_file_is_not_found() {
    let server = TestServer::start("plan-no-file").await;
    server.add_user("alice", "correct horse", None).await;
    let tv = server.client().await;
    tv.hello_with(None, Some(ClientCapabilities::default()))
        .await;
    let alice = tv.login("alice", "correct horse").await;
    let request = Request::PlaybackPlan {
        id: movie().id().clone(),
        selection: Default::default(),
        bitmap: Default::default(),
    };
    assert!(matches!(
        tv.call(Some(&alice), request).await,
        Response::Err(Error::NotFound { .. })
    ));
    tv.close().await;
    server.stop().await;
}

/// "Asked for on" and session rows name the kind of client each device
/// runs, as its Hello said (P2-08, P3-03).
#[tokio::test]
async fn devices_and_sessions_say_what_kind_of_client_each_device_runs() {
    let server = TestServer::start("device-platform").await;
    server.add_user("alice", "correct horse", None).await;
    let tv = server.client().await;
    let desktop = server.client().await;
    assert!(matches!(
        tv.hello_from(Platform::Tvos, Some("Living room TV"), None)
            .await,
        Response::Ok(Reply::ServerInfo(_))
    ));
    desktop.hello().await;
    let on_tv = tv.login("alice", "correct horse").await;
    let on_desktop = desktop.login("alice", "correct horse").await;

    let tv_id = tv.endpoint.id().to_string();
    let Response::Ok(Reply::Devices { devices }) =
        desktop.call(Some(&on_desktop), Request::ListDevices).await
    else {
        panic!("no devices");
    };
    let platform_of = |id: &str| {
        devices
            .iter()
            .find(|d| d.endpoint_id == id)
            .unwrap()
            .platform
    };
    assert_eq!(platform_of(&tv_id), Some(Platform::Tvos));
    assert_eq!(
        platform_of(&desktop.endpoint.id().to_string()),
        Some(Platform::Linux)
    );

    let Response::Ok(Reply::Sessions { sessions }) =
        tv.call(Some(&on_tv), Request::ListSessions).await
    else {
        panic!("no sessions");
    };
    let mut platforms: Vec<_> = sessions.iter().map(|s| (s.current, s.platform)).collect();
    platforms.sort_by_key(|(current, _)| *current);
    assert_eq!(
        platforms,
        [(false, Some(Platform::Linux)), (true, Some(Platform::Tvos))]
    );
    tv.close().await;
    desktop.close().await;
    server.stop().await;
}

/// A device signed in before the server kept platforms has none, until it
/// next connects and says Hello.
#[tokio::test]
async fn a_device_from_before_platforms_were_kept_gets_one_when_it_connects() {
    let server = TestServer::start("device-platform-later").await;
    server.add_user("alice", "correct horse", None).await;
    let tv = server.client().await;
    tv.hello().await;
    let session = tv.login("alice", "correct horse").await;
    let tv_id = tv.endpoint.id().to_string();
    // As migration 0011 leaves a device it did not know.
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new()
            .filename(server.dir.as_ref().join("spoolframe.db")),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE devices SET platform = NULL")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let platform = |client: support::TestClient| {
        let session = session.clone();
        let tv_id = tv_id.clone();
        async move {
            let Response::Ok(Reply::Devices { devices }) =
                client.call(Some(&session), Request::ListDevices).await
            else {
                panic!("no devices");
            };
            devices
                .into_iter()
                .find(|d| d.endpoint_id == tv_id)
                .unwrap()
                .platform
        }
    };
    assert_eq!(platform(tv.clone()).await, None);
    let tv = tv.reconnect(&server).await;
    assert_eq!(platform(tv.clone()).await, Some(Platform::Linux));
    tv.close().await;
    server.stop().await;
}
