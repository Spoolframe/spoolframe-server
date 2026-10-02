//! The server endpoint, dialled by a real Iroh client endpoint over
//! loopback with relays off.

use crate::support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use iroh::endpoint::{ApplicationClose, ConnectionError, VarInt, presets};
use iroh::{Endpoint, RelayMode};
use spoolframe_protocol::{CLIENT_ALPN, FEDERATION_ALPN, close};
use spoolframe_server::network::{ConnectionAuth, Server};

fn state_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("endpoint")
        .join(test);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn server(state_dir: &Path) -> Server {
    support::TestServer::start_in(state_dir.to_owned())
        .await
        .server
}

async fn client() -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await
        .unwrap()
}

/// Wait until `check` holds, or fail after a few seconds.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..100 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn a_client_connection_is_anonymous_and_tracked_until_it_closes() {
    let server = server(&state_dir("client")).await;
    let client = client().await;
    let conn = client.connect(server.addr(), CLIENT_ALPN).await.unwrap();

    let expected = ConnectionAuth::Anonymous {
        endpoint: client.id(),
    };
    eventually("the connection to be tracked", || {
        server.connections().snapshot() == [expected.clone()]
    })
    .await;

    conn.close(VarInt::from_u32(0), b"done");
    eventually("the connection to be forgotten", || {
        server.connections().snapshot().is_empty()
    })
    .await;
    client.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn federation_is_refused_with_a_reason() {
    let server = server(&state_dir("federation")).await;
    let client = client().await;
    let conn = client
        .connect(server.addr(), FEDERATION_ALPN)
        .await
        .unwrap();
    let ConnectionError::ApplicationClosed(ApplicationClose { error_code, reason }) =
        conn.closed().await
    else {
        panic!("not closed by the server");
    };
    assert_eq!(error_code, VarInt::from_u32(close::UNSUPPORTED_PROTOCOL));
    let reason = String::from_utf8_lossy(&reason);
    assert!(reason.contains("spoolframe/federation/1"), "{reason}");
    assert!(server.connections().snapshot().is_empty());
    client.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn an_unknown_alpn_is_refused() {
    let server = server(&state_dir("unknown-alpn")).await;
    let client = client().await;
    let result = client.connect(server.addr(), b"spoolframe/client/2").await;
    assert!(result.is_err(), "connected with an unknown ALPN");
    client.close().await;
    server.shutdown().await;
}

#[tokio::test]
async fn a_restart_keeps_the_endpoint_id() {
    let dir = state_dir("restart");
    let first = server(&dir).await;
    let id = first.id();
    first.shutdown().await;
    let second = server(&dir).await;
    assert_eq!(second.id(), id);
    second.shutdown().await;
}
