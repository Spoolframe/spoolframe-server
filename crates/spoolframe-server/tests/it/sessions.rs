//! Sessions bound to the endpoint that logged in, several per connection
//! (P1-12).

use crate::support;

use spoolframe_protocol::{Error, PlaybackPlan, Reply, Request, Response, UserSummary};
use spoolframe_types::ClientCapabilities;
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
