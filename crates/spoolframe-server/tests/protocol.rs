//! `spoolframe/client/1` as a client sees it, over a real Iroh connection.

mod support;

use std::time::Duration;

use serde::Serialize;
use spoolframe_protocol::codec::read_frame;
use spoolframe_protocol::streams::uni;
use spoolframe_protocol::{
    AuthMethod, Error, Event, LoginPassword, PROTOCOL_VERSION, Reply, Request, Response, SessionId,
};
use spoolframe_server::auth::MAX_FAILURES;
use spoolframe_types::{LibraryId, MediaId, MediaKind, Namespace};
use support::{TestClient, TestServer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A server and one client that has not said Hello yet.
async fn connect(test: &str) -> (TestServer, TestClient) {
    let server = TestServer::start(test).await;
    let client = server.client().await;
    (server, client)
}

async fn close(server: TestServer, client: TestClient) {
    client.close().await;
    server.stop().await;
}

fn login(username: &str, password: &str) -> Request {
    Request::LoginPassword(LoginPassword {
        username: username.into(),
        password: password.into(),
    })
}

#[tokio::test]
async fn hello_is_answered_with_server_info() {
    let (server, client) = connect("hello").await;
    let Response::Ok(Reply::ServerInfo(info)) = client.hello_as(None).await else {
        panic!("no ServerInfo");
    };
    assert_eq!(info.name, "Home Media");
    assert_eq!(info.endpoint_id, server.server.id().to_string());
    assert_eq!(info.protocol_version, PROTOCOL_VERSION);
    assert_eq!(info.auth_methods, [AuthMethod::Password]);
    close(server, client).await;
}

#[tokio::test]
async fn nothing_is_served_before_hello() {
    let (server, client) = connect("before-hello").await;
    assert_eq!(
        client.call(None, Request::Ping).await,
        Response::Err(Error::HandshakeRequired)
    );
    close(server, client).await;
}

#[tokio::test]
async fn ping_and_auth_methods_need_no_session() {
    let (server, client) = connect("pre-auth").await;
    client.hello().await;
    assert_eq!(
        client.call(None, Request::Ping).await,
        Response::Ok(Reply::Pong)
    );
    assert_eq!(
        client.call(None, Request::AuthMethods).await,
        Response::Ok(Reply::AuthMethods {
            methods: vec![AuthMethod::Password]
        })
    );
    close(server, client).await;
}

#[tokio::test]
async fn the_right_password_opens_a_session() {
    let (server, client) = connect("login").await;
    server.add_user("alice", "correct horse", None).await;
    client.hello().await;
    let session = client.login("alice", "correct horse").await;
    assert_eq!(session.0.len(), 64);
    close(server, client).await;
}

#[tokio::test]
async fn a_wrong_password_and_an_unknown_user_look_the_same() {
    let (server, client) = connect("bad-login").await;
    server.add_user("alice", "correct horse", None).await;
    client.hello().await;
    for (user, password) in [("alice", "battery staple"), ("mallory", "correct horse")] {
        assert_eq!(
            client.call(None, login(user, password)).await,
            Response::Err(Error::InvalidCredentials),
            "{user}"
        );
    }
    close(server, client).await;
}

#[tokio::test]
async fn repeated_failures_from_an_endpoint_are_rate_limited() {
    let (server, client) = connect("rate-limit").await;
    server.add_user("alice", "correct horse", None).await;
    client.hello().await;
    for _ in 0..MAX_FAILURES {
        client.call(None, login("alice", "wrong guess")).await;
    }
    let response = client.call(None, login("alice", "correct horse")).await;
    assert!(
        matches!(response, Response::Err(Error::RateLimited { retry_after_ms }) if retry_after_ms > 0),
        "{response:?}"
    );
    close(server, client).await;
}

#[tokio::test]
async fn every_other_request_needs_a_session() {
    let (server, client) = connect("unauthenticated").await;
    client.hello().await;
    let id = MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap();
    let gated = [
        Request::Libraries,
        Request::Items {
            library: LibraryId::new(Namespace::Plex, "1").unwrap(),
            offset: 0,
            limit: 10,
        },
        Request::Item { id: id.clone() },
        Request::Search {
            query: "alien".into(),
            limit: 10,
        },
        Request::Tracks { id },
        Request::WhoAmI,
        Request::Logout,
        Request::ListSessions,
        Request::PlaybackPlan {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
            selection: Default::default(),
        },
    ];
    let made_up = SessionId("made-up".into());
    for request in gated {
        for session in [None, Some(&made_up)] {
            assert_eq!(
                client.call(session, request.clone()).await,
                Response::Err(Error::Unauthenticated),
                "{request:?} with {session:?}"
            );
        }
    }
    close(server, client).await;
}

#[tokio::test]
async fn an_unknown_request_is_unsupported_and_the_connection_stays_up() {
    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum Newer {
        RedeemInvite { code: String },
    }
    #[derive(Serialize)]
    struct Envelope {
        session: Option<String>,
        request: Newer,
    }

    let (server, client) = connect("unknown-request").await;
    client.hello().await;
    let response = client
        .send(&Envelope {
            session: None,
            request: Newer::RedeemInvite { code: "abc".into() },
        })
        .await;
    assert_eq!(
        response,
        Response::Err(Error::Unsupported {
            request: "redeem_invite".into()
        })
    );
    assert_eq!(
        client.call(None, Request::Ping).await,
        Response::Ok(Reply::Pong)
    );
    close(server, client).await;
}

#[tokio::test]
async fn a_stream_of_unknown_kind_is_refused_and_the_connection_stays_up() {
    let (server, client) = connect("unknown-stream").await;
    client.hello().await;
    let (mut send, mut recv) = client.conn.open_bi().await.unwrap();
    send.write_u8(0x7f).await.unwrap();
    send.finish().unwrap();
    assert!(recv.read_to_end(64).await.is_err(), "stream was not reset");
    assert_eq!(
        client.call(None, Request::Ping).await,
        Response::Ok(Reply::Pong)
    );
    close(server, client).await;
}

#[tokio::test]
async fn several_requests_can_run_at_once() {
    let (server, client) = connect("concurrent").await;
    client.hello().await;
    let calls: Vec<_> = (0..8)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move { client.call(None, Request::Ping).await })
        })
        .collect();
    for call in calls {
        assert_eq!(call.await.unwrap(), Response::Ok(Reply::Pong));
    }
    close(server, client).await;
}

#[tokio::test]
async fn the_server_says_it_is_shutting_down() {
    let (server, client) = connect("shutdown").await;
    client.hello().await;
    let mut events = client.conn.accept_uni().await.unwrap();
    assert_eq!(events.read_u8().await.unwrap(), uni::EVENTS);

    let shutdown = tokio::spawn(server.stop());
    let event: Event = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut events))
        .await
        .expect("no event before the timeout")
        .unwrap();
    assert_eq!(event, Event::ShuttingDown);
    shutdown.await.unwrap();
    client.endpoint.close().await;
}
