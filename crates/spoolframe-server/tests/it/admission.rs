//! Who may hold the server's connections, streams and buffers (PX-10,
//! findings 7 and C-3): a device with no live session gets small limits,
//! a signed-in one the normal ones, and a device that signs out is closed.

use crate::support;

use std::time::Duration;

use iroh::endpoint::{Connection, ConnectionError, ReadError, VarInt};
use spoolframe_protocol::streams::{bi, reset};
use spoolframe_protocol::{Error, LoginPassword, Reply, Request, Response, close};
use spoolframe_server::admission::{Admission, STRANGER_BI_STREAMS};
use spoolframe_server::auth::{Bucket, SessionToken};
use support::{ServerOptions, TestServer};

/// Long enough for anything the server does at once on loopback; a wait
/// that ends by this has not happened, and will not.
const AT_ONCE: Duration = Duration::from_secs(5);

/// How long an open of a stream the peer gives no credit for is given:
/// credit comes in the handshake, so an open either returns at once or
/// never.
const NO_CREDIT: Duration = Duration::from_millis(300);

async fn server_with(test: &str, admission: Admission) -> TestServer {
    TestServer::start_with(
        support::fresh_dir(test).await,
        ServerOptions {
            admission,
            ..ServerOptions::default()
        },
    )
    .await
}

/// `count` bidirectional streams, opened and held, each within
/// [`AT_ONCE`]; how many opened.
async fn open_bi_streams(conn: &Connection, count: usize, wait: Duration) -> Vec<impl Sized> {
    let mut held = Vec::new();
    for _ in 0..count {
        match tokio::time::timeout(wait, conn.open_bi()).await {
            Ok(Ok(stream)) => held.push(stream),
            _ => break,
        }
    }
    held
}

/// The code the server closed `conn` with, waiting at most [`AT_ONCE`].
async fn closed_with(conn: &Connection) -> Option<u32> {
    match tokio::time::timeout(AT_ONCE, conn.closed()).await {
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

#[tokio::test]
async fn a_stranger_can_open_no_uni_stream_and_only_a_few_bi_streams() {
    let server = TestServer::start("stranger-streams").await;
    // Before Hello, so no stream it used is still counted.
    let client = server.client().await;
    assert!(
        tokio::time::timeout(NO_CREDIT, client.conn.open_uni())
            .await
            .is_err(),
        "a stranger opened a uni stream"
    );
    let held = open_bi_streams(&client.conn, STRANGER_BI_STREAMS as usize + 1, NO_CREDIT).await;
    assert_eq!(held.len(), STRANGER_BI_STREAMS as usize);
    drop(held);
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_signed_in_device_gets_more_streams_on_this_connection_and_the_next() {
    let server = TestServer::start("known-streams").await;
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello().await;
    client.login("alice", "correct horse").await;
    let many = 4 * STRANGER_BI_STREAMS as usize;
    let held = open_bi_streams(&client.conn, many, AT_ONCE).await;
    assert_eq!(held.len(), many, "on the connection it signed in on");
    drop(held);

    let again = client.reconnect(&server).await;
    let held = open_bi_streams(&again.conn, many, AT_ONCE).await;
    assert_eq!(held.len(), many, "on a new connection");
    // Events go the other way: no device opens a uni stream.
    assert!(
        tokio::time::timeout(NO_CREDIT, again.conn.open_uni())
            .await
            .is_err()
    );
    drop(held);
    again.close().await;
    server.stop().await;
}

#[tokio::test]
async fn past_the_cap_a_stranger_is_refused_and_a_signed_in_device_is_not() {
    let server = server_with(
        "stranger-cap",
        Admission {
            max_strangers: 2,
            ..Admission::default()
        },
    )
    .await;
    server.add_user("alice", "correct horse", None).await;
    let device = server.client().await;
    device.hello().await;
    device.login("alice", "correct horse").await;
    device.conn.close(0u32.into(), b"later");

    let first = server.client().await;
    first.hello().await;
    let second = server.client().await;
    second.hello().await;
    let third = server.client().await;
    assert_eq!(closed_with(&third.conn).await, Some(close::BUSY));

    // A known device is let in at once, past the cap.
    let back = device.reconnect(&server).await;
    assert_eq!(
        back.call(None, Request::Ping).await,
        Response::Ok(Reply::Pong)
    );
    // And a stranger again once one goes.
    first.close().await;
    tokio::time::timeout(AT_ONCE, async {
        // `second` and `back`.
        while server.server.connections().snapshot().len() > 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the stranger that went is still held");
    let next = server.client().await;
    next.hello().await;
    server.stop().await;
}

#[tokio::test]
async fn logging_out_closes_the_connection_after_its_reply() {
    let server = TestServer::start("logout-closes").await;
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello().await;
    let session = client.login("alice", "correct horse").await;
    assert_eq!(
        client.call(Some(&session), Request::Logout).await,
        Response::Ok(Reply::LoggedOut)
    );
    assert_eq!(closed_with(&client.conn).await, Some(close::SIGNED_OUT));
    // It may come back, as a stranger.
    let again = client.reconnect(&server).await;
    assert_eq!(
        again.call(None, Request::Ping).await,
        Response::Ok(Reply::Pong)
    );
    server.stop().await;
}

#[tokio::test]
async fn revoking_a_device_s_session_elsewhere_closes_its_connection() {
    let server = TestServer::start("revoke-closes").await;
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello().await;
    let session = client.login("alice", "correct horse").await;
    server
        .store
        .revoke_session(&SessionToken(session.0).hash())
        .await
        .unwrap();
    assert_eq!(closed_with(&client.conn).await, Some(close::SIGNED_OUT));
    server.stop().await;
}

#[tokio::test]
async fn disabling_the_user_closes_their_device_s_connection() {
    let server = TestServer::start("disable-closes").await;
    server.add_user("alice", "correct horse", None).await;
    server.add_user("bob", "battery staple", None).await;
    let client = server.client().await;
    client.hello().await;
    client.login("alice", "correct horse").await;
    server.store.disable_user("alice").await.unwrap();
    assert_eq!(closed_with(&client.conn).await, Some(close::SIGNED_OUT));
    server.stop().await;
}

#[tokio::test]
async fn a_device_stays_connected_while_another_of_its_sessions_is_live() {
    let server = TestServer::start("two-sessions").await;
    server.add_user("alice", "correct horse", None).await;
    server.add_user("bob", "battery staple", None).await;
    let client = server.client().await;
    client.hello().await;
    let alice = client.login("alice", "correct horse").await;
    let bob = client.login("bob", "battery staple").await;
    assert_eq!(
        client.call(Some(&alice), Request::Logout).await,
        Response::Ok(Reply::LoggedOut)
    );
    assert!(
        matches!(
            client.call(Some(&bob), Request::WhoAmI).await,
            Response::Ok(_)
        ),
        "bob's session on the same device"
    );
    assert_eq!(
        client.call(Some(&bob), Request::Logout).await,
        Response::Ok(Reply::LoggedOut)
    );
    assert_eq!(closed_with(&client.conn).await, Some(close::SIGNED_OUT));
    server.stop().await;
}

#[tokio::test]
async fn a_stream_whose_first_frame_stalls_is_reset_and_the_connection_stays() {
    let server = server_with(
        "first-frame",
        Admission {
            first_frame: Duration::from_millis(200),
            ..Admission::default()
        },
    )
    .await;
    let client = server.client().await;
    client.hello().await;
    let (mut send, mut recv) = client.conn.open_bi().await.unwrap();
    // The kind and half a length, then nothing.
    send.write_all(&[bi::RPC, 0, 0]).await.unwrap();
    let mut buf = [0u8; 16];
    let read = tokio::time::timeout(AT_ONCE, recv.read(&mut buf))
        .await
        .expect("the stalled stream was never reset");
    assert!(
        matches!(read, Err(ReadError::Reset(code)) if code == VarInt::from_u32(reset::FIRST_FRAME_TIMEOUT)),
        "{read:?}"
    );
    assert_eq!(
        client.call(None, Request::Ping).await,
        Response::Ok(Reply::Pong)
    );
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

/// PX-09 finding 9 (A-4): sign-in attempts from free endpoint ids are
/// held to one rate for all of them; a signed-in device is not.
#[tokio::test]
async fn strangers_share_one_sign_in_budget_and_a_signed_in_device_does_not() {
    let server = TestServer::start_with(
        support::fresh_dir("stranger-attempts").await,
        ServerOptions {
            stranger_attempts: Bucket {
                capacity: 3,
                refill_every: Duration::from_secs(3600),
            },
            ..ServerOptions::default()
        },
    )
    .await;
    server.add_user("alice", "correct horse", None).await;
    server.add_user("bob", "battery staple", None).await;
    let device = server.client().await;
    device.hello().await;
    device.login("alice", "correct horse").await;
    for _ in 0..2 {
        let stranger = server.client().await;
        stranger.hello().await;
        assert_eq!(
            stranger.call(None, login("mallory", "guess")).await,
            Response::Err(Error::InvalidCredentials)
        );
    }
    let fresh = server.client().await;
    fresh.hello().await;
    assert!(
        matches!(
            fresh.call(None, login("bob", "battery staple")).await,
            Response::Err(Error::RateLimited { .. })
        ),
        "a fourth stranger's attempt was let through"
    );
    // A second user on the signed-in device is not a stranger's attempt.
    device.login("bob", "battery staple").await;
    server.stop().await;
}

/// Third review G-14: a device that signed out is a stranger again while
/// its streams drain before the close, and its sign-in attempts draw on
/// the strangers' budget; they drew on nothing for up to 2 s.
#[tokio::test]
async fn a_signed_out_device_s_sign_ins_draw_on_the_strangers_budget_while_it_drains() {
    // The device's own sign-in takes the only token, for an hour.
    const REFILL: Duration = Duration::from_secs(3600);
    let server = TestServer::start_with(
        support::fresh_dir("signed-out-stranger-attempts").await,
        ServerOptions {
            stranger_attempts: Bucket {
                capacity: 1,
                refill_every: REFILL,
            },
            ..ServerOptions::default()
        },
    )
    .await;
    server.add_user("alice", "correct horse", None).await;
    let device = server.client().await;
    device.hello().await;
    let session = device.login("alice", "correct horse").await;
    // A stream waiting for its first frame holds the drain open.
    let (mut held, _recv) = device.conn.open_bi().await.unwrap();
    held.write_all(&[bi::RPC]).await.unwrap();
    assert_eq!(
        device.call(Some(&session), Request::Logout).await,
        Response::Ok(Reply::LoggedOut)
    );
    // The sign-out lands soon after the reply, from a task of the
    // server's: until the connection closes, each wrong guess is refused
    // by the failure limit (15 minutes) or, once the device is a stranger,
    // by the strangers' budget (an hour).
    let mut by_budget = false;
    for guess in 0.. {
        if device.conn.close_reason().is_some() {
            break;
        }
        // A call on a closed connection panics: that ends the drain too.
        let attempt = tokio::spawn({
            let device = device.clone();
            async move {
                device
                    .call(None, login(&format!("guess-{guess}"), "wrong"))
                    .await
            }
        });
        let Ok(Ok(response)) = tokio::time::timeout(AT_ONCE, attempt).await else {
            break;
        };
        if let Response::Err(Error::RateLimited { retry_after_ms }) = response
            && retry_after_ms > 1_800_000
        {
            by_budget = true;
            break;
        }
    }
    assert!(
        by_budget,
        "no sign-in of the signed-out device drew on the strangers' budget"
    );
    server.stop().await;
}

/// PX-09 finding 16 (A-6): an app passkey sign-in is held per endpoint,
/// and endpoint ids are free, so strangers' beginnings draw on their
/// shared sign-in budget too: the 10,000 table cannot be filled in its
/// 5 minutes.
#[tokio::test]
async fn strangers_beginning_passkey_sign_ins_share_their_budget() {
    let server = TestServer::start_with(
        support::fresh_dir("stranger-passkey-begin").await,
        ServerOptions {
            stranger_attempts: Bucket {
                capacity: 2,
                refill_every: Duration::from_secs(3600),
            },
            ..ServerOptions::default()
        },
    )
    .await;
    let begin = Request::BeginPasskeySignIn { username: None };
    for _ in 0..2 {
        let stranger = server.client().await;
        stranger.hello().await;
        assert!(matches!(
            stranger.call(None, begin.clone()).await,
            Response::Ok(Reply::PasskeySignInBegun(_))
        ));
    }
    let another = server.client().await;
    another.hello().await;
    assert!(
        matches!(
            another.call(None, begin).await,
            Response::Err(Error::RateLimited { .. })
        ),
        "a third stranger began a passkey sign-in"
    );
    server.stop().await;
}
