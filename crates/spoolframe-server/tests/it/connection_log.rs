//! Which connections the server's log names at INFO. An app that dials,
//! says Hello and hangs up without signing in is DEBUG: the dev server's
//! log had 20,000 such pairs from one device. A connection that was signed
//! in, or ended in a way worth a look, is still INFO.

use std::time::Duration;

use crate::support::{LogCapture, Logged, TestServer, alone};

const PASSWORD: &str = "battery staple";

/// The server's INFO events so far: lines, not spans.
fn info_lines(capture: &LogCapture) -> Vec<Logged> {
    capture
        .saying_anything()
        .into_iter()
        .filter(|line| line.level == tracing::Level::INFO && line.target.starts_with("spoolframe"))
        .collect()
}

/// The server's "client disconnected" line, once it has seen the close.
async fn disconnected(capture: &LogCapture) -> Logged {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(line) = capture.saying("client disconnected").pop() {
                return line;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the server never saw the connection close")
}

#[tokio::test]
async fn an_anonymous_connection_that_hangs_up_is_not_info() {
    if !alone("connection_log::an_anonymous_connection_that_hangs_up_is_not_info") {
        return;
    }
    let server = TestServer::start("log-anonymous").await;
    let (capture, _guard) = LogCapture::start();
    let phone = server.client().await;
    phone.hello().await;
    phone.close().await;

    let line = disconnected(&capture).await;
    assert_eq!(line.level, tracing::Level::DEBUG, "{line:#?}");
    let connected = capture.saying("client connected");
    assert_eq!(connected.len(), 1, "{connected:#?}");
    assert_eq!(connected[0].level, tracing::Level::DEBUG);
    let info = info_lines(&capture);
    assert!(info.is_empty(), "{info:#?}");
    server.stop().await;
}

/// What the dev server saw: a device with a live session dials, says
/// Hello, and hangs up without using it. It is `known`, but this
/// connection never signed in.
#[tokio::test]
async fn a_known_device_that_hangs_up_without_its_session_is_not_info() {
    if !alone("connection_log::a_known_device_that_hangs_up_without_its_session_is_not_info") {
        return;
    }
    let server = TestServer::start("log-known").await;
    server.add_user("alice", PASSWORD, None).await;
    let phone = server.client().await;
    phone.hello().await;
    phone.login("alice", PASSWORD).await;
    let (capture, _guard) = LogCapture::start();
    let again = phone.reconnect(&server).await;
    again.conn.close(0u32.into(), b"done");
    let line = disconnected(&capture).await;
    assert_eq!(line.level, tracing::Level::DEBUG, "{line:#?}");
    let info = info_lines(&capture);
    assert!(info.is_empty(), "{info:#?}");
    phone.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_signed_in_connection_s_disconnect_is_info() {
    if !alone("connection_log::a_signed_in_connection_s_disconnect_is_info") {
        return;
    }
    let server = TestServer::start("log-signed-in").await;
    server.add_user("alice", PASSWORD, None).await;
    let (capture, _guard) = LogCapture::start();
    let phone = server.client().await;
    phone.hello().await;
    phone.login("alice", PASSWORD).await;
    phone.close().await;

    let line = disconnected(&capture).await;
    assert_eq!(line.level, tracing::Level::INFO, "{line:#?}");
    assert_eq!(line.field("signed_in"), Some("true"), "{line:#?}");
    // The paths it went by: this machine's own, none a public one.
    let paths = line.field("paths").unwrap_or_default();
    assert!(
        paths.split(" -> ").all(|path| path.starts_with("lan ")),
        "{line:#?}"
    );
    assert_eq!(capture.saying("client disconnected").len(), 1);
    assert_eq!(capture.saying("logged in").len(), 1);
    server.stop().await;
}

/// A session used on a connection makes it a signed-in one, as a sign-in
/// on it does: an app that kept its session never signs in again.
#[tokio::test]
async fn a_connection_that_uses_a_kept_session_is_info() {
    if !alone("connection_log::a_connection_that_uses_a_kept_session_is_info") {
        return;
    }
    let server = TestServer::start("log-kept-session").await;
    server.add_user("alice", PASSWORD, None).await;
    let phone = server.client().await;
    phone.hello().await;
    let session = phone.login("alice", PASSWORD).await;
    let (capture, _guard) = LogCapture::start();
    let again = phone.reconnect(&server).await;
    let me = again
        .call(Some(&session), spoolframe_protocol::Request::WhoAmI)
        .await;
    assert!(matches!(me, spoolframe_protocol::Response::Ok(_)), "{me:?}");
    again.conn.close(0u32.into(), b"done");
    let line = disconnected(&capture).await;
    assert_eq!(line.level, tracing::Level::INFO, "{line:#?}");
    phone.close().await;
    server.stop().await;
}
