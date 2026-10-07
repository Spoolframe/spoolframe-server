//! A saved server remembers every address it was reached at (PX-12), not
//! only the paths of the last connection: a later connection over one
//! address adds to them and never replaces them, so a device that once
//! reached the server another way can still dial it that way.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use iroh::SecretKey;
use spoolframe_client_sdk::storage::keys;
use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{Client, ClientOptions, Device, MemoryStorage, Storage};

use crate::stub::Stub;

/// No test takes this long unless something hangs.
const HANG: Duration = Duration::from_secs(60);

async fn within<T>(test: impl Future<Output = T>) -> T {
    tokio::time::timeout(HANG, test)
        .await
        .expect("the test hung")
}

fn device() -> Device {
    Device {
        name: "phone".into(),
        client_name: "sdk-address-test".into(),
        client_version: "0".into(),
        platform: Platform::Ios,
        capabilities: ClientCapabilities::default(),
    }
}

/// This device, as the app opens it again: what it knows of servers is
/// only what `storage` kept. It does not listen on the LAN, so nothing a
/// real server there announces is mixed into what was saved.
async fn open(storage: &Arc<MemoryStorage>) -> Client {
    let options = ClientOptions {
        lan_discovery: false,
        ..ClientOptions::default()
    };
    Client::open_with_options(storage.clone(), device(), options)
        .await
        .unwrap()
}

/// The addresses saved for server `id`, read by a client that has not
/// dialled it.
async fn saved_addrs(storage: &Arc<MemoryStorage>, id: &str) -> Vec<SocketAddr> {
    let client = open(storage).await;
    let entry = client
        .servers()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.endpoint_id == id)
        .expect("the server is saved");
    client.close().await;
    entry.addrs
}

/// A stub server on loopback, on IPv4 and IPv6: two addresses, of which
/// a client is given only the first.
async fn two_address_stub(key: &SecretKey) -> (Stub, SocketAddr, SocketAddr) {
    let stub = Stub::start_on(
        key.clone(),
        &["127.0.0.1:0".parse().unwrap(), "[::1]:0".parse().unwrap()],
    )
    .await;
    let bound = stub.bound();
    let v4 = *bound.iter().find(|a| a.is_ipv4()).unwrap();
    let v6 = *bound.iter().find(|a| a.is_ipv6()).unwrap();
    (stub, v4, v6)
}

/// Pin `stub`, given only `addr`, and wait until the client has seen the
/// server at `also` too: iroh opens a path there on its own, once the
/// server has told it its addresses.
async fn pin_via(storage: &Arc<MemoryStorage>, stub: &Stub, addr: SocketAddr, also: SocketAddr) {
    let client = open(storage).await;
    client.add_address(&stub.id(), vec![addr]).unwrap();
    client.connect(&stub.id()).await.unwrap();
    client.pin(&stub.id(), true).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let entry = client
            .servers()
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.endpoint_id == stub.id())
            .unwrap();
        if entry.addrs.contains(&also) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the client never listed {also}: {:?}",
            entry.addrs
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    client.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_connection_saves_every_address_it_reached_the_server_at() {
    within(async {
        let key = SecretKey::generate();
        let (stub, v4, v6) = two_address_stub(&key).await;
        let storage = Arc::new(MemoryStorage::default());
        pin_via(&storage, &stub, v4, v6).await;
        stub.stop().await;

        let saved = saved_addrs(&storage, &key.public().to_string()).await;
        assert!(
            saved.contains(&v4) && saved.contains(&v6),
            "saved {saved:?}, not both of {v4} and {v6}"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_over_one_address_keeps_the_others_to_dial_later() {
    within(async {
        let key = SecretKey::generate();
        let id = key.public().to_string();
        let storage = Arc::new(MemoryStorage::default());
        let (stub, v4, v6) = two_address_stub(&key).await;
        pin_via(&storage, &stub, v4, v6).await;
        stub.stop().await;

        // The server is reached over IPv4 alone, as a phone away from home
        // reaches it over one address.
        let stub = Stub::start_on(key.clone(), &[v4]).await;
        let client = open(&storage).await;
        client.connect(&id).await.unwrap();
        client.close().await;
        stub.stop().await;

        // Then it answers only on IPv6, which no one gave this client:
        // it is dialled from what was saved.
        let stub = Stub::start_on(key, &[v6]).await;
        let client = open(&storage).await;
        let entry = client
            .connect(&id)
            .await
            .expect("the server is dialled at the address saved before");
        assert_eq!(entry.addrs.first(), Some(&v6), "the one in use comes first");
        client.close().await;
        stub.stop().await;
    })
    .await;
}

/// One server, as a phone sees it: reached at `v4`, and also at `v6`,
/// where this device has never reached it (its VPN address, say). The
/// same key answers at each, from two stubs: the one at `v4` pushes both
/// addresses to a signed-in device, and iroh never learns `v6` on its own.
async fn told_pair(key: &SecretKey) -> (Stub, Stub, SocketAddr, SocketAddr) {
    let at_v4 = Stub::start_on(key.clone(), &["127.0.0.1:0".parse().unwrap()]).await;
    let at_v6 = Stub::start_on(key.clone(), &["[::1]:0".parse().unwrap()]).await;
    let v4 = at_v4.bound()[0];
    let v6 = at_v6.bound()[0];
    at_v4.tell(&[v4, v6]);
    (at_v4, at_v6, v4, v6)
}

/// Wait until `client` has saved `addr` for server `id`.
async fn until_saved(client: &Client, id: &str, addr: SocketAddr) {
    until_addrs(client, id, &format!("{addr} saved"), |addrs| {
        addrs.contains(&addr)
    })
    .await;
}

/// Wait until the addresses `client` saved for server `id` are `what`.
async fn until_addrs(
    client: &Client,
    id: &str,
    what: &str,
    done: impl Fn(&[SocketAddr]) -> bool,
) -> Vec<SocketAddr> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let entry = client
            .servers()
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.endpoint_id == id)
            .expect("the server is saved");
        if done(&entry.addrs) {
            return entry.addrs;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never {what}: {:?}",
            entry.addrs
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Pin the server at `v4` alone and sign in there.
async fn signed_in_via(storage: &Arc<MemoryStorage>, stub: &Stub, v4: SocketAddr) -> Client {
    let client = open(storage).await;
    client.add_address(&stub.id(), vec![v4]).unwrap();
    client.connect(&stub.id()).await.unwrap();
    client.pin(&stub.id(), true).await.unwrap();
    client
        .sign_in(&stub.id(), "alice", "correct horse", false)
        .await
        .unwrap();
    client
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sign_in_saves_the_addresses_the_server_pushes_without_asking() {
    within(async {
        let key = SecretKey::generate();
        let (at_v4, at_v6, v4, v6) = told_pair(&key).await;
        let storage = Arc::new(MemoryStorage::default());
        let client = signed_in_via(&storage, &at_v4, v4).await;
        until_saved(&client, &at_v4.id(), v6).await;
        client.close().await;

        assert_eq!(at_v4.count("server_addresses"), 0, "the device asked");
        assert_eq!(at_v6.connections(), 0, "the device reached {v6} itself");
        at_v4.stop().await;
        at_v6.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_later_connect_over_only_a_told_address_succeeds() {
    within(async {
        let key = SecretKey::generate();
        let id = key.public().to_string();
        let (at_v4, at_v6, v4, v6) = told_pair(&key).await;
        let storage = Arc::new(MemoryStorage::default());
        let client = signed_in_via(&storage, &at_v4, v4).await;
        until_saved(&client, &id, v6).await;
        client.close().await;
        // Away from home: the address it was reached at is gone.
        at_v4.stop().await;

        let client = open(&storage).await;
        let entry = client
            .connect(&id)
            .await
            .expect("the server is dialled at the address it told");
        assert_eq!(entry.addrs.first(), Some(&v6), "the one in use comes first");
        client.close().await;
        at_v6.stop().await;
    })
    .await;
}

/// An app opened again with a session kept is pushed the addresses on its
/// first connection, so one the server has gained since is learnt without
/// a sign-in.
#[tokio::test(flavor = "multi_thread")]
async fn a_kept_session_is_pushed_them_on_the_next_connection() {
    within(async {
        let key = SecretKey::generate();
        let id = key.public().to_string();
        let (at_v4, at_v6, v4, v6) = told_pair(&key).await;
        at_v4.tell(&[v4]);
        let storage = Arc::new(MemoryStorage::default());
        let client = signed_in_via(&storage, &at_v4, v4).await;
        client.close().await;

        at_v4.tell(&[v4, v6]);
        let client = open(&storage).await;
        client.connect(&id).await.unwrap();
        until_saved(&client, &id, v6).await;
        client.close().await;
        at_v4.stop().await;
        at_v6.stop().await;
    })
    .await;
}

/// A second push replaces what the first told: an address the server no
/// longer tells (a Docker bridge it now ignores) is gone at once, and the
/// one this device reached stays though the server leaves it out.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_push_replaces_the_told_addresses_and_keeps_the_reached_one() {
    within(async {
        let key = SecretKey::generate();
        let id = key.public().to_string();
        let stub = Stub::start_on(key, &["127.0.0.1:0".parse().unwrap()]).await;
        let v4 = stub.bound()[0];
        let bridge: SocketAddr = "172.17.0.1:4433".parse().unwrap();
        let vpn: SocketAddr = "10.10.0.2:4433".parse().unwrap();
        stub.tell(&[v4, bridge]);
        let storage = Arc::new(MemoryStorage::default());
        let client = signed_in_via(&storage, &stub, v4).await;
        until_saved(&client, &id, bridge).await;

        stub.tell(&[vpn]);
        stub.push().await;
        let saved = until_addrs(&client, &id, "the bridge forgotten", |addrs| {
            !addrs.contains(&bridge)
        })
        .await;
        assert!(saved.contains(&vpn), "{saved:?}");
        assert!(saved.contains(&v4), "the address reached went: {saved:?}");
        client.close().await;
        stub.stop().await;
    })
    .await;
}

/// The relays a push lists are saved with the server.
#[tokio::test(flavor = "multi_thread")]
async fn the_relays_pushed_are_saved() {
    within(async {
        let key = SecretKey::generate();
        let stub = Stub::start_on(key, &["127.0.0.1:0".parse().unwrap()]).await;
        let v4 = stub.bound()[0];
        stub.tell(&[v4]);
        stub.tell_relays(&["https://relay.example.net./"]);
        let storage = Arc::new(MemoryStorage::default());
        let client = signed_in_via(&storage, &stub, v4).await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let file = storage.data(keys::SERVERS).await.unwrap().unwrap();
            let servers: serde_json::Value = serde_json::from_slice(&file).unwrap();
            if servers[0]["relays"] == serde_json::json!(["https://relay.example.net./"]) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the relays were never saved: {servers}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        client.close().await;
        stub.stop().await;
    })
    .await;
}

/// An app shows what the server advertises apart from every dial hint:
/// the latest pushed list, in the server's order, with its relays and
/// when it came. An address only reached is not in it.
#[tokio::test(flavor = "multi_thread")]
async fn the_entry_shows_exactly_what_the_server_advertised_last() {
    within(async {
        let key = SecretKey::generate();
        let id = key.public().to_string();
        let stub = Stub::start_on(key, &["127.0.0.1:0".parse().unwrap()]).await;
        let v4 = stub.bound()[0];
        let vpn: SocketAddr = "10.10.0.2:4433".parse().unwrap();
        let lan: SocketAddr = "192.168.86.10:4433".parse().unwrap();
        stub.tell(&[vpn, lan]);
        stub.tell_relays(&["https://relay.example.net./"]);
        let storage = Arc::new(MemoryStorage::default());
        let before = std::time::SystemTime::now();
        let client = signed_in_via(&storage, &stub, v4).await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let entry = loop {
            let entry = client
                .servers()
                .await
                .unwrap()
                .into_iter()
                .find(|s| s.endpoint_id == id)
                .expect("the server is saved");
            if !entry.advertised_addrs.is_empty() {
                break entry;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "nothing advertised: {entry:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(entry.advertised_addrs, [vpn, lan]);
        assert_eq!(entry.relays, ["https://relay.example.net./"]);
        assert!(entry.addrs.contains(&v4), "{:?}", entry.addrs);
        let at = entry.advertised_at.expect("when it was advertised");
        // Kept in whole seconds.
        assert!(
            at + Duration::from_secs(1) >= before,
            "{at:?} before {before:?}"
        );

        // A second push replaces it, in its order.
        stub.tell(&[lan]);
        stub.tell_relays(&[]);
        stub.push().await;
        let addrs = loop {
            let entry = client
                .servers()
                .await
                .unwrap()
                .into_iter()
                .find(|s| s.endpoint_id == id)
                .unwrap();
            if entry.advertised_addrs.len() == 1 {
                assert_eq!(entry.relays, Vec::<String>::new());
                break entry.advertised_addrs;
            }
            assert!(tokio::time::Instant::now() < deadline, "{entry:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(addrs, [lan]);
        client.close().await;
        stub.stop().await;
    })
    .await;
}
