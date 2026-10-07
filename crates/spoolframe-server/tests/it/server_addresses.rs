//! A signed-in device is told where the server can be dialled directly
//! (PX-12): every address its endpoint has on its interfaces, LAN, public
//! and VPN, so that a phone that only ever reached it one way still learns
//! the others. The server pushes them on the events stream (revision 7).
//! A stranger is told nothing of them.

use crate::support;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::RecvStream;
use spoolframe_client_sdk::MemoryStorage;
use spoolframe_protocol::codec::read_frame;
use spoolframe_protocol::streams::uni;
use spoolframe_protocol::{Error, Event, Request, Response};
use spoolframe_server::announce::{Announce, Glob, VIRTUAL_INTERFACES, interfaces};
use support::{ServerOptions, TestClient, TestServer, fresh_dir, mkv_capabilities};
use tokio::io::AsyncReadExt;

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

/// Of [`reachable`], what a default `network.announce` tells: none on
/// this machine's virtual interfaces (its docker0, its bridges).
fn told_by_default(server: &TestServer) -> BTreeSet<SocketAddr> {
    let virtual_globs: Vec<Glob> = VIRTUAL_INTERFACES
        .iter()
        .map(|g| g.parse().unwrap())
        .collect();
    let on_virtual: Vec<std::net::IpAddr> = interfaces()
        .into_iter()
        .filter(|i| virtual_globs.iter().any(|g| g.matches(&i.name)))
        .flat_map(|i| i.addrs)
        .map(|ip| ip.to_canonical())
        .collect();
    reachable(server)
        .into_iter()
        .filter(|a| !on_virtual.contains(&a.ip().to_canonical()))
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

/// What `client` is pushed once it says Hello and signs in as alice: the
/// addresses, in the order told.
async fn told(client: &TestClient) -> Vec<SocketAddr> {
    client.hello().await;
    let mut events = events(client).await;
    client.login("alice", PASSWORD).await;
    match next_event(&mut events, Duration::from_secs(5)).await {
        Some(Event::ServerAddresses { addrs, .. }) => addrs,
        other => panic!("not the server's addresses: {other:?}"),
    }
}

/// The events stream the server opened at `client`'s Hello.
async fn events(client: &TestClient) -> RecvStream {
    let mut events = tokio::time::timeout(Duration::from_secs(5), client.conn.accept_uni())
        .await
        .expect("no events stream within 5 s")
        .unwrap();
    assert_eq!(events.read_u8().await.unwrap(), uni::EVENTS);
    events
}

/// The next event on `events`, if one comes within `wait`.
async fn next_event(events: &mut RecvStream, wait: Duration) -> Option<Event> {
    tokio::time::timeout(wait, read_frame::<_, Event>(events))
        .await
        .ok()
        .map(|event| event.expect("an event that decodes"))
}

/// The addresses of a `ServerAddresses` event, with its relays.
fn pushed(event: Option<Event>) -> (BTreeSet<SocketAddr>, Vec<String>) {
    match event {
        Some(Event::ServerAddresses { addrs, relays }) => {
            let set: BTreeSet<SocketAddr> = addrs.iter().copied().collect();
            assert_eq!(set.len(), addrs.len(), "each once: {addrs:?}");
            (set, relays)
        }
        other => panic!("not the server's addresses: {other:?}"),
    }
}

/// Signed in, a connection is pushed the addresses without asking, and
/// no relay while relays are off.
#[tokio::test]
async fn a_signed_in_connection_is_pushed_the_addresses_and_no_relays() {
    let server = server("addrs-pushed").await;
    let client = server.client().await;
    client.hello().await;
    let mut events = events(&client).await;
    client.login("alice", PASSWORD).await;

    let event = next_event(&mut events, Duration::from_secs(5)).await;
    assert_eq!(pushed(event), (told_by_default(&server), Vec::new()));
    server.stop().await;
}

/// A connection with no session is never pushed them, though another
/// device signs in meanwhile and is.
#[tokio::test]
async fn an_anonymous_connection_is_pushed_nothing() {
    let server = server("addrs-pushed-anonymous").await;
    let stranger = server.client().await;
    stranger.hello().await;
    let mut strangers = events(&stranger).await;

    let device = server.client().await;
    device.hello().await;
    let mut devices = events(&device).await;
    device.login("alice", PASSWORD).await;
    pushed(next_event(&mut devices, Duration::from_secs(5)).await);

    assert_eq!(
        next_event(&mut strangers, Duration::from_secs(1)).await,
        None
    );
    server.stop().await;
}

/// A device that kept its session is pushed them on its next connection,
/// right after Hello, with no sign-in on it.
#[tokio::test]
async fn a_kept_session_is_pushed_the_addresses_on_reconnect() {
    let server = server("addrs-pushed-reconnect").await;
    let device = server.client().await;
    device.hello().await;
    device.login("alice", PASSWORD).await;

    let again = device.reconnect(&server).await;
    let mut events = events(&again).await;
    let event = next_event(&mut events, Duration::from_secs(5)).await;
    assert_eq!(pushed(event).0, told_by_default(&server));
    server.stop().await;
}

/// A device signed out since is pushed nothing on its next connection,
/// though it was before.
#[tokio::test]
async fn a_device_signed_out_is_pushed_nothing_on_its_next_connection() {
    let server = server("addrs-signed-out").await;
    let device = server.client().await;
    device.hello().await;
    device.login("alice", PASSWORD).await;
    let again = device.reconnect(&server).await;
    pushed(next_event(&mut events(&again).await, Duration::from_secs(5)).await);

    server.store.disable_user("alice").await.unwrap();
    // The server closes the device's connections once its last session
    // ends (PX-10), and one made before that lands is closed with them:
    // reconnect only after.
    let closed = tokio::time::timeout(Duration::from_secs(10), again.conn.closed())
        .await
        .expect("the server closes a signed-out device's connection");
    assert!(
        matches!(
            &closed,
            iroh::endpoint::ConnectionError::ApplicationClosed(close)
                if close.error_code
                    == iroh::endpoint::VarInt::from_u32(spoolframe_protocol::close::SIGNED_OUT)
        ),
        "{closed:?}"
    );
    let after = device.reconnect(&server).await;
    assert_eq!(
        next_event(&mut events(&after).await, Duration::from_secs(1)).await,
        None
    );
    server.stop().await;
}

/// The addresses say nothing about the user: a locked profile is pushed
/// them too, before any PIN is entered.
#[tokio::test]
async fn a_locked_profile_is_pushed_them_too() {
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
    let mut events = events(&restarted).await;
    assert_eq!(
        restarted.call(Some(&session), Request::Libraries).await,
        Response::Err(Error::ProfileLocked),
        "the profile is not locked"
    );
    let event = next_event(&mut events, Duration::from_secs(5)).await;
    assert_eq!(pushed(event).0, told_by_default(&server));
    server.stop().await;
}

/// The SDK, signed in over one address, saves every one the server tells.
#[tokio::test]
async fn the_sdk_saves_every_address_it_is_told_after_a_sign_in() {
    let server = server("addrs-sdk").await;
    let expected = told_by_default(&server);
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
    let addrs = told(&client).await;
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
    assert_eq!(told(&client).await, []);
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
    assert_eq!(
        told(&client).await,
        [
            SocketAddr::from(([203, 0, 113, 7], port)),
            "198.51.100.4:443".parse().unwrap(),
        ]
    );
    server.stop().await;
}
