//! A signed-in device is told where the server can be dialled directly
//! (PX-12): every address its endpoint has on its interfaces, LAN, public
//! and VPN, so that a phone that only ever reached it one way still learns
//! the others. A stranger is told nothing of them.

use crate::support;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::MemoryStorage;
use spoolframe_protocol::{Error, Reply, Request, Response, SessionId};
use spoolframe_server::announce::Announce;
use support::{ServerOptions, TestClient, TestServer, fresh_dir, mkv_capabilities};

const PASSWORD: &str = "correct horse";

async fn server(test: &str) -> TestServer {
    let server = TestServer::start(test).await;
    server.add_user("alice", PASSWORD, Some("Alice")).await;
    server
}

/// What the server's endpoint can be dialled at, less what no other
/// device can reach: loopback and link-local.
fn reachable(server: &TestServer) -> BTreeSet<SocketAddr> {
    server
        .server
        .addr()
        .ip_addrs()
        .copied()
        .filter(|a| {
            let ip = a.ip().to_canonical();
            !ip.is_loopback()
                && match ip {
                    std::net::IpAddr::V4(v4) => !v4.is_link_local(),
                    std::net::IpAddr::V6(v6) => !v6.is_unicast_link_local(),
                }
        })
        .collect()
}

async fn announcing(test: &str, announce: Announce) -> TestServer {
    let options = ServerOptions {
        announce,
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(fresh_dir(test).await, options).await;
    server.add_user("alice", PASSWORD, Some("Alice")).await;
    server
}

async fn told(client: &TestClient, session: Option<&SessionId>) -> Result<Vec<SocketAddr>, Error> {
    match client.call(session, Request::ServerAddresses).await {
        Response::Ok(Reply::ServerAddresses { addrs }) => Ok(addrs),
        Response::Ok(other) => panic!("ServerAddresses answered {other:?}"),
        Response::Err(e) => Err(e),
    }
}

#[tokio::test]
async fn a_signed_in_device_is_told_every_address_but_loopback_and_link_local() {
    let server = server("addrs-signed-in").await;
    let expected = reachable(&server);
    assert!(
        !expected.is_empty(),
        "this machine has no address but loopback"
    );
    let client = server.client().await;
    client.hello().await;
    let session = client.login("alice", PASSWORD).await;

    let addrs = told(&client, Some(&session)).await.unwrap();
    assert_eq!(addrs.iter().copied().collect::<BTreeSet<_>>(), expected);
    assert_eq!(addrs.len(), expected.len(), "each once: {addrs:?}");
    server.stop().await;
}

#[tokio::test]
async fn a_stranger_is_not_told_the_addresses() {
    let server = server("addrs-stranger").await;
    let stranger = server.client().await;
    stranger.hello().await;
    assert_eq!(told(&stranger, None).await, Err(Error::Unauthenticated));
    let made_up = SessionId("not a session".into());
    assert_eq!(
        told(&stranger, Some(&made_up)).await,
        Err(Error::Unauthenticated)
    );

    // Nor is a device signed out since.
    let device = server.client().await;
    device.hello().await;
    let session = device.login("alice", PASSWORD).await;
    server.store.disable_user("alice").await.unwrap();
    let again = server.client().await;
    again.hello().await;
    assert_eq!(
        told(&again, Some(&session)).await,
        Err(Error::Unauthenticated)
    );
    server.stop().await;
}

/// The addresses say nothing about the user, and the SDK asks for them
/// right after a sign-in, before any PIN is entered.
#[tokio::test]
async fn a_locked_profile_is_told_them_too() {
    let server = server("addrs-locked").await;
    let tv = server.client().await;
    tv.hello().await;
    let session = tv.login("alice", PASSWORD).await;
    for request in [
        Request::SetPin {
            pin: "4321".into(),
            proof: None,
        },
        Request::SetPinRequired {
            device: None,
            required: true,
        },
    ] {
        assert!(
            matches!(tv.call(Some(&session), request).await, Response::Ok(_)),
            "setting the PIN up failed"
        );
    }
    let restarted = tv.reconnect(&server).await;
    assert_eq!(
        restarted.call(Some(&session), Request::Libraries).await,
        Response::Err(Error::ProfileLocked),
        "the profile is not locked"
    );
    assert_eq!(
        told(&restarted, Some(&session))
            .await
            .unwrap()
            .into_iter()
            .collect::<BTreeSet<_>>(),
        reachable(&server)
    );
    server.stop().await;
}

/// The SDK, signed in over one address, saves every one the server tells.
#[tokio::test]
async fn the_sdk_saves_every_address_it_is_told_after_a_sign_in() {
    let server = server("addrs-sdk").await;
    let expected = reachable(&server);
    let first = *expected.iter().next().expect("an address to sign in over");
    let storage = Arc::new(MemoryStorage::default());
    let device = spoolframe_client_sdk::Device {
        name: "phone".into(),
        client_name: "sdk-test".into(),
        client_version: "0".into(),
        platform: spoolframe_client_sdk::types::Platform::Ios,
        capabilities: mkv_capabilities(),
    };
    let client = crate::support::sdk_client(storage, device).await.unwrap();
    let id = server.server.id().to_string();
    client.add_address(&id, vec![first]).unwrap();
    client.connect(&id).await.unwrap();
    client.pin(&id, true).await.unwrap();
    client.sign_in(&id, "alice", PASSWORD, false).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let saved: BTreeSet<SocketAddr> = client
            .servers()
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.endpoint_id == id)
            .expect("the server is saved")
            .addrs
            .into_iter()
            .collect();
        if saved.is_superset(&expected) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "saved {saved:?}, not every one of {expected:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    client.close().await;
    server.stop().await;
}

/// `network.announce.interfaces` finds each address's interface on this
/// machine: allowing every one tells every address, and ignoring every
/// one tells none of them.
#[tokio::test]
async fn the_addresses_told_follow_the_interfaces_they_are_on() {
    let every = Announce {
        interfaces: vec!["*".parse().unwrap()],
        ..Announce::default()
    };
    let server = announcing("addrs-every-interface", every).await;
    let client = server.client().await;
    client.hello().await;
    let session = client.login("alice", PASSWORD).await;
    let addrs = told(&client, Some(&session)).await.unwrap();
    assert_eq!(
        addrs.into_iter().collect::<BTreeSet<_>>(),
        reachable(&server)
    );
    server.stop().await;

    let none = Announce {
        ignore_interfaces: vec!["*".parse().unwrap()],
        ..Announce::default()
    };
    let server = announcing("addrs-no-interface", none).await;
    let client = server.client().await;
    client.hello().await;
    let session = client.login("alice", PASSWORD).await;
    assert_eq!(told(&client, Some(&session)).await.unwrap(), []);
    server.stop().await;
}

/// An address the host does not see, as a NAT's, is told with the
/// endpoint's port when it names none, though every range is ignored.
#[tokio::test]
async fn added_addresses_are_told_with_the_endpoint_port() {
    let announce = Announce {
        add: vec![
            "203.0.113.7".parse().unwrap(),
            "198.51.100.4:443".parse().unwrap(),
        ],
        ignore: vec!["0.0.0.0/0".parse().unwrap(), "::/0".parse().unwrap()],
        ..Announce::default()
    };
    let server = announcing("addrs-added", announce).await;
    let port = server
        .server
        .addr()
        .ip_addrs()
        .find(|a| a.is_ipv4())
        .expect("an IPv4 address")
        .port();
    let client = server.client().await;
    client.hello().await;
    let session = client.login("alice", PASSWORD).await;
    assert_eq!(
        told(&client, Some(&session)).await.unwrap(),
        [
            SocketAddr::from(([203, 0, 113, 7], port)),
            "198.51.100.4:443".parse().unwrap(),
        ]
    );
    server.stop().await;
}
