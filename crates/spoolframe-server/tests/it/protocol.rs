//! `spoolframe/client/1` as a client sees it, over a real Iroh connection.

use crate::support;

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
        remember: false,
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
    assert_eq!(info.server_id, server.server_id.as_str());
    assert_eq!(info.protocol_version, PROTOCOL_VERSION);
    assert_eq!(
        info.auth_methods,
        [AuthMethod::Password, AuthMethod::DeviceApproval]
    );
    close(server, client).await;
}

#[tokio::test]
async fn hello_says_where_the_web_ui_is_when_it_has_a_public_url() {
    let (server, client) = connect("hello-no-web").await;
    let Response::Ok(Reply::ServerInfo(info)) = client.hello_as(None).await else {
        panic!("no ServerInfo");
    };
    assert_eq!(info.web_url, None);
    close(server, client).await;

    let server = TestServer::start_with(
        support::fresh_dir("hello-web").await,
        support::ServerOptions {
            public_url: Some("https://media.example.org".into()),
            ..Default::default()
        },
    )
    .await;
    let client = server.client().await;
    let Response::Ok(Reply::ServerInfo(info)) = client.hello_as(None).await else {
        panic!("no ServerInfo");
    };
    assert_eq!(info.web_url.as_deref(), Some("https://media.example.org"));
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
            methods: vec![AuthMethod::Password, AuthMethod::DeviceApproval]
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
        Request::Search(support::search("alien")),
        Request::Tracks { id },
        Request::WhoAmI,
        Request::Logout,
        Request::ListSessions,
        Request::PlaybackPlan {
            id: MediaId::new(Namespace::Plex, MediaKind::Movie, "1").unwrap(),
            selection: Default::default(),
            bitmap: Default::default(),
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

/// A request is named, in its span, its refusal log line and an
/// `Unsupported` reply, by one name; a client's type is at most 64
/// characters of it, on one line (C-12).
#[tokio::test]
async fn a_long_unknown_type_is_named_in_64_characters_on_one_line() {
    #[derive(Serialize)]
    struct Tagged {
        #[serde(rename = "type")]
        kind: String,
    }
    #[derive(Serialize)]
    struct Envelope {
        session: Option<String>,
        request: Tagged,
    }

    let (server, client) = connect("long-unknown-type").await;
    client.hello().await;
    let response = client
        .send(&Envelope {
            session: None,
            request: Tagged {
                kind: "line\n".repeat(800),
            },
        })
        .await;
    let Response::Err(Error::Unsupported { request }) = response else {
        panic!("{response:?}");
    };
    // Ten escaped lines of six characters, and four of the next.
    assert_eq!(request, format!("{}line", r"line\n".repeat(10)));
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

#[tokio::test]
async fn unauthenticated_mutations_cannot_exhaust_replay_capacity() {
    let (server, client) = connect("unauthenticated-capacity").await;
    server.add_user("alice", "correct horse", None).await;
    client.hello().await;
    for _ in 0..4100 {
        assert_eq!(
            client.call(None, Request::Logout).await,
            Response::Err(Error::Unauthenticated)
        );
    }
    let session = client.login("alice", "correct horse").await;
    assert_eq!(
        client.call(Some(&session), Request::Logout).await,
        Response::Ok(Reply::LoggedOut)
    );
    close(server, client).await;
}

/// PX-09 finding 17 (C-6): an anonymous device's Hello strings are kept
/// and shown to approvers, so each is cut to a bound.
#[tokio::test]
async fn hello_strings_are_cut_to_their_bound() {
    let (server, client) = connect("hello-strings").await;
    let long = "é".repeat(10_000);
    let hello = spoolframe_protocol::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: long.clone(),
        client_version: long.clone(),
        platform: spoolframe_types::Platform::Linux,
        device_name: Some(long.clone()),
        capabilities: None,
    };
    assert!(matches!(
        client.call(None, Request::Hello(hello)).await,
        Response::Ok(Reply::ServerInfo(_))
    ));
    let devices = server.debug.connected.devices();
    let device = &devices[0];
    let cut = "é".repeat(spoolframe_protocol::MAX_HELLO_TEXT);
    // Compared as booleans: a failure must not print 10,000 characters.
    assert!(device.client_name == cut, "client_name");
    assert!(device.client_version == cut, "client_version");
    assert!(
        device.device_name.as_deref() == Some(cut.as_str()),
        "device_name"
    );
    close(server, client).await;
}
