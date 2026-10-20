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

/// Keep `addr`'s port from any other socket until dropped.
fn hold(addr: SocketAddr) -> std::net::UdpSocket {
    std::net::UdpSocket::bind(addr).expect("the freed port is free")
}

/// A stub for `key` at exactly `addr`.
async fn start_at(key: &SecretKey, addr: SocketAddr) -> Stub {
    let stub = Stub::start_on(key.clone(), &[addr]).await;
    assert_eq!(stub.bound(), vec![addr], "the stub is at {addr}");
    stub
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
        // While no stub is at an address, a socket holds its port: the
        // other tests bind ports of their own meanwhile, and one given
        // this freed port would answer, or keep the next stub off it.
        let held_v6 = hold(v6);

        // The server is reached over IPv4 alone, as a phone away from home
        // reaches it over one address.
        let stub = start_at(&key, v4).await;
        let client = open(&storage).await;
        client.connect(&id).await.unwrap();
        client.close().await;
        stub.stop().await;
        let _held_v4 = hold(v4);

        // Then it answers only on IPv6, which no one gave this client:
        // it is dialled from what was saved.
        drop(held_v6);
        let stub = start_at(&key, v6).await;
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

/// A playback's share of the server's uplink (PX-46) reaches the app
/// with the bitrate the server gave it, not only its handle: an app can
/// say what its playback may take.
#[tokio::test(flavor = "multi_thread")]
async fn a_pushed_share_reaches_the_app_with_its_bitrate() {
    within(async {
        let key = SecretKey::generate();
        let id = key.public().to_string();
        let stub = Stub::start_on(key, &["127.0.0.1:0".parse().unwrap()]).await;
        let v4 = stub.bound()[0];
        let bridge: SocketAddr = "172.17.0.1:4433".parse().unwrap();
        stub.tell(&[v4, bridge]);
        let storage = Arc::new(MemoryStorage::default());
        let client = signed_in_via(&storage, &stub, v4).await;
        // The events stream is open once the first push is saved.
        until_saved(&client, &id, bridge).await;
        let mut shares = client.playback_shares();
        stub.push_share("h1", Some(4_500_000)).await;
        let share = shares.recv().await.unwrap();
        assert_eq!(share.handle.0, "h1");
        assert_eq!(share.max_bitrate, Some(4_500_000));
        client.close().await;
        stub.stop().await;
    })
    .await;
}

/// A second push replaces what the first told: an address the server no
/// longer tells (a Docker bridge it now ignores) is gone at once, and the
/// one this device last reached it at stays though the server leaves it
/// out.
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

/// A UDP forwarder to `target`, each packet to it `delay` late: a path to
/// the server slower than one to another address. Its address, and the
/// task to abort when done.
async fn slow_path(
    target: SocketAddr,
    delay: Duration,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    use std::collections::HashMap;
    use tokio::net::UdpSocket;

    let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = front.local_addr().unwrap();
    let task = tokio::spawn(async move {
        // One socket towards the server for each client socket, so that
        // its answers go back to where the packet came from.
        let mut backs: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
        let mut buf = vec![0u8; 65_536];
        while let Ok((n, from)) = front.recv_from(&mut buf).await {
            let back = match backs.get(&from) {
                Some(back) => back.clone(),
                None => {
                    let back = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
                    let (answers, front) = (back.clone(), front.clone());
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 65_536];
                        while let Ok((n, _)) = answers.recv_from(&mut buf).await {
                            let _ = front.send_to(&buf[..n], from).await;
                        }
                    });
                    backs.insert(from, back.clone());
                    back
                }
            };
            let packet = buf[..n].to_vec();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = back.send_to(&packet, target).await;
            });
        }
    });
    (addr, task)
}

/// A servers file as the Linux client had it for Fulcrum: reached last at
/// `stale`, a port that a server of another key has taken since, and told
/// that `current` is where it is now.
async fn poisoned(storage: &Arc<MemoryStorage>, id: &str, stale: SocketAddr, current: SocketAddr) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let file = serde_json::json!([{
        "name": "Fulcrum",
        "endpoint_id": id,
        "known_addrs": [
            {"addr": stale, "last_seen": now, "source": "reached"},
            {"addr": current, "last_seen": now - 60, "source": "told"},
        ],
        "verified": true,
        "advertised_addrs": [current],
        "advertised_at": now - 60,
        // No relays or lookups: the dial is given only the two addresses.
        "use_relays": false,
    }]);
    storage
        .set_data(keys::SERVERS, Some(serde_json::to_vec(&file).unwrap()))
        .await
        .unwrap();
}

/// The Linux client's Fulcrum (2026-10-16): another server answers first,
/// with its own key, at an address where this device once reached the
/// server, and the server is at the address it tells. Iroh settles the
/// handshake on the first answer, so a dial given both fails outright.
/// The server's list leaves that address out, and it is not the last
/// path, so it is not kept, and the dial reaches the server.
#[tokio::test(flavor = "multi_thread")]
async fn another_key_at_an_address_reached_before_cannot_stop_the_dial() {
    within(async {
        let key = SecretKey::generate();
        let id = key.public().to_string();
        let server = Stub::start_on(key, &["127.0.0.1:0".parse().unwrap()]).await;
        // It answers half a second after the other: the other wins any race.
        let (current, forwarder) = slow_path(server.bound()[0], Duration::from_millis(500)).await;
        let other = Stub::start_on(SecretKey::generate(), &["[::1]:0".parse().unwrap()]).await;
        let stale = other.bound()[0];
        let storage = Arc::new(MemoryStorage::default());
        poisoned(&storage, &id, stale, current).await;

        let client = open(&storage).await;
        client
            .connect(&id)
            .await
            .expect("the server is reached at the address it tells");
        client.close().await;
        forwarder.abort();
        server.stop().await;
        other.stop().await;
    })
    .await;
}

/// A port forward the server was never told of (no `network.wan`): this
/// device reaches the server only at an address its list leaves out. That
/// address carried the last connection, so it is kept and dialled.
#[tokio::test(flavor = "multi_thread")]
async fn an_unadvertised_address_that_last_worked_is_dialled() {
    within(async {
        let key = SecretKey::generate();
        let id = key.public().to_string();
        let stub = Stub::start_on(key, &["127.0.0.1:0".parse().unwrap()]).await;
        let forwarded = stub.bound()[0];
        // What the server advertises: nothing answers there from here.
        let gone = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let advertised = gone.local_addr().unwrap();
        stub.tell(&[advertised]);
        let storage = Arc::new(MemoryStorage::default());
        let client = signed_in_via(&storage, &stub, forwarded).await;
        until_saved(&client, &id, advertised).await;
        client.close().await;

        let client = open(&storage).await;
        client
            .connect(&id)
            .await
            .expect("the server is reached where it was last");
        client.close().await;
        stub.stop().await;
    })
    .await;
}

/// The address that carried the last connection, where another key
/// answers now (the port went to another server), and the server is away
/// from where it advertises it is: the dial fails at the handshake, and
/// the address is let go. Once the server is back where it advertises, it
/// is reached there, and the other key is not dialled at all.
#[tokio::test(flavor = "multi_thread")]
async fn a_last_path_where_another_key_answers_now_is_let_go() {
    within(async {
        let key = SecretKey::generate();
        let id = key.public().to_string();
        let other = Stub::start_on(SecretKey::generate(), &["[::1]:0".parse().unwrap()]).await;
        let stale = other.bound()[0];
        // The server is away from where it advertises it is, for now.
        let gone = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let away = gone.local_addr().unwrap();
        let storage = Arc::new(MemoryStorage::default());
        poisoned(&storage, &id, stale, away).await;
        let file = storage.data(keys::SERVERS).await.unwrap().unwrap();
        let mut servers: serde_json::Value = serde_json::from_slice(&file).unwrap();
        servers[0]["last_path"] = serde_json::json!(stale);
        storage
            .set_data(keys::SERVERS, Some(serde_json::to_vec(&servers).unwrap()))
            .await
            .unwrap();
        assert!(
            saved_addrs(&storage, &id).await.contains(&stale),
            "the last path is kept until it fails"
        );

        let client = open(&storage).await;
        client
            .connect(&id)
            .await
            .expect_err("another key answers at the last path");
        client.close().await;
        let file = storage.data(keys::SERVERS).await.unwrap().unwrap();
        let servers: serde_json::Value = serde_json::from_slice(&file).unwrap();
        assert_eq!(servers[0].get("last_path"), None, "{servers}");
        assert!(
            !servers[0]["known_addrs"]
                .to_string()
                .contains(&stale.to_string()),
            "let go: {servers}"
        );

        // The server is back where it advertises it is.
        drop(gone);
        let server = start_at(&key, away).await;
        let client = open(&storage).await;
        client
            .connect(&id)
            .await
            .expect("the server is reached at the address it tells");
        assert_eq!(
            other.connections(),
            0,
            "the other key was handed a connection"
        );
        client.close().await;
        server.stop().await;
        other.stop().await;
    })
    .await;
}
