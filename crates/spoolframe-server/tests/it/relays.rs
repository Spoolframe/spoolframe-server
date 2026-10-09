//! Relays (P6-03): a server with `network.relays` takes its home relay
//! from them and tells it first, and a device with relays on reaches the
//! server through it alone: the path when nothing can dial the server
//! directly. The relay is a real iroh relay on loopback, over plain HTTP.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use iroh_relay::server::{RelayConfig, Server as RelayServer, ServerConfig};
use spoolframe_client_sdk::{Client, ClientOptions, Device, MemoryStorage, SdkError};
use spoolframe_protocol::invite::Invite;
use spoolframe_server::network::Relay;

use crate::support::{self, ServerOptions, TestServer};

/// A relay on loopback, and its URL.
async fn relay() -> (RelayServer, url::Url) {
    let mut config = ServerConfig::default();
    config.relay = Some(RelayConfig::new((Ipv4Addr::LOCALHOST, 0)));
    let relay = RelayServer::spawn(config).await.unwrap();
    let addr = relay.http_addr().expect("served over HTTP");
    (relay, url::Url::parse(&format!("http://{addr}")).unwrap())
}

/// A server whose only relay is `url`, once it has taken it as its home
/// relay: until then it cannot be reached through it.
async fn server_on(test: &str, url: &url::Url) -> TestServer {
    let options = ServerOptions {
        relays: vec![Relay::Custom(url.clone())],
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(support::fresh_dir(test).await, options).await;
    let told = server.server.told();
    let home = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if is_home(&server, url) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(home.is_ok(), "no home relay: {:?}", told.relays());
    server
}

/// Whether the server's endpoint has `url` as its home relay now.
fn is_home(server: &TestServer, url: &url::Url) -> bool {
    let url = iroh::RelayUrl::from(url.clone());
    server.server.addr().relay_urls().any(|u| *u == url)
}

/// A link to `server` with its relays and no address: only a relay can
/// reach it.
fn relay_only_link(server: &TestServer) -> String {
    Invite {
        server: server.server.id().to_string(),
        relays: server.server.told().relays(),
        ..Invite::default()
    }
    .link()
}

async fn device(relays: bool) -> Client {
    let options = ClientOptions {
        lan_discovery: false,
        relays,
        ..ClientOptions::default()
    };
    Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        Device {
            name: "Den PC".into(),
            client_name: "sdk-test".into(),
            client_version: "0".into(),
            platform: spoolframe_client_sdk::types::Platform::Linux,
            capabilities: Default::default(),
        },
        options,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn the_server_tells_its_home_relay() {
    let (relay, url) = relay().await;
    let server = server_on("relay-told", &url).await;
    assert_eq!(server.server.told().relays(), [url.to_string()]);
    server.stop().await;
    drop(relay);
}

#[tokio::test]
async fn a_device_with_relays_on_reaches_the_server_through_its_relay_alone() {
    let (relay, url) = relay().await;
    let server = server_on("relay-reached", &url).await;
    let client = device(true).await;
    let entry = client
        .connect_invite(&relay_only_link(&server))
        .await
        .unwrap();
    assert_eq!(entry.name, "Home Media");
    client.close().await;
    server.stop().await;
    drop(relay);
}

#[tokio::test]
async fn a_device_with_relays_off_never_reaches_a_server_through_one() {
    let (relay, url) = relay().await;
    let server = server_on("relay-off", &url).await;
    let client = device(false).await;
    let refused = client.connect_invite(&relay_only_link(&server)).await;
    assert!(
        matches!(refused, Err(SdkError::Unreachable(_))),
        "{refused:?}"
    );
    client.close().await;
    server.stop().await;
    drop(relay);
}

/// The ticket's LAN check: relays that cannot be reached cost nothing to
/// a device that can dial the server directly.
#[tokio::test]
async fn a_relay_that_cannot_be_reached_does_not_stop_a_direct_connection() {
    // The discard port: nothing answers there.
    let dead = url::Url::parse("http://127.0.0.1:9/").unwrap();
    let options = ServerOptions {
        relays: vec![Relay::Custom(dead.clone())],
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(support::fresh_dir("relay-dead").await, options).await;
    assert_eq!(server.server.told().relays(), [dead.to_string()]);
    let link = Invite {
        server: server.server.id().to_string(),
        addrs: server.server.addr().ip_addrs().copied().collect(),
        relays: server.server.told().relays(),
        ..Invite::default()
    }
    .link();
    let client = device(true).await;
    let entry = client.connect_invite(&link).await.unwrap();
    assert_eq!(entry.name, "Home Media");
    client.close().await;
    server.stop().await;
}
