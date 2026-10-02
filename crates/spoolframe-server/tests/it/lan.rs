//! LAN discovery over mDNS (P1-13).
//!
//! Each test runs itself again inside a private network namespace
//! ([`private_lan`]) whose only interface is loopback, with multicast on.
//! There, every record is from this test: no other Spoolframe server on the
//! real LAN is heard, and the host firewall, which usually drops inbound
//! mDNS (UDP 5353), is not in the way.

use crate::support;

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use iroh::endpoint::presets;
use iroh::endpoint_info::UserData;
use iroh::{Endpoint, EndpointId, RelayMode, SecretKey};
use iroh_mdns_address_lookup::{DiscoveryEvent, MdnsAddressLookup};
use spoolframe_client_sdk::{Client, Device, MemoryStorage, ServerEntry};
use spoolframe_protocol::lan;
use spoolframe_types::ClientCapabilities;
use support::{ServerOptions, TestServer, fresh_dir};

/// How long a record may take to be heard. mDNS answers within a second
/// or two on a quiet host; this is the bound, not the expected time.
const HEAR_WITHIN: Duration = Duration::from_secs(20);

/// A listen-only mDNS browser on one service name, recording the user data
/// of every endpoint it hears.
struct Listener {
    heard: Arc<Mutex<HashMap<EndpointId, Option<String>>>>,
    _mdns: MdnsAddressLookup,
}

impl Listener {
    async fn on(service: &str) -> Self {
        let mdns = MdnsAddressLookup::builder()
            .advertise(false)
            .service_name(service)
            .build(SecretKey::generate().public())
            .unwrap();
        let mut events = mdns.subscribe().await;
        let heard = Arc::new(Mutex::new(HashMap::new()));
        let record = heard.clone();
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                if let DiscoveryEvent::Discovered { endpoint_info, .. } = event {
                    let data = endpoint_info.data.user_data().map(|u| u.to_string());
                    record
                        .lock()
                        .unwrap()
                        .insert(endpoint_info.endpoint_id, data);
                }
            }
        });
        Self { heard, _mdns: mdns }
    }

    fn get(&self, id: &EndpointId) -> Option<Option<String>> {
        self.heard.lock().unwrap().get(id).cloned()
    }

    /// The user data `id` published, once it is heard.
    async fn hear(&self, id: &EndpointId) -> Option<String> {
        until(&format!("{id} heard"), || async { self.get(id) }).await
    }
}

/// Poll `check` until it gives a value, for at most [`HEAR_WITHIN`].
async fn until<T, F, Fut>(what: &str, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    tokio::time::timeout(HEAR_WITHIN, async {
        loop {
            if let Some(value) = check().await {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("not within {HEAR_WITHIN:?}: {what}"))
}

fn device() -> Device {
    Device {
        name: "laptop".into(),
        client_name: "lan-test".into(),
        client_version: "0".into(),
        capabilities: ClientCapabilities::default(),
    }
}

async fn open(storage: &Arc<MemoryStorage>) -> Client {
    Client::open(storage.clone(), device()).await.unwrap()
}

/// The client's entry for `server`, once it has one that `ready` accepts.
async fn entry(
    client: &Client,
    server: &TestServer,
    ready: impl Fn(&ServerEntry) -> bool,
) -> ServerEntry {
    let id = server.server.id().to_string();
    until(&format!("an entry for {id}"), || async {
        client
            .servers()
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.endpoint_id == id && ready(e))
    })
    .await
}

/// A bare Iroh endpoint announcing itself on `service` with `user_data`.
async fn decoy(service: &str, user_data: Option<&str>) -> Endpoint {
    let mut builder = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .address_lookup(MdnsAddressLookup::builder().service_name(service));
    if let Some(data) = user_data {
        builder = builder.user_data_for_address_lookup(data.parse::<UserData>().unwrap());
    }
    builder.bind().await.unwrap()
}

/// Whether this process is the one inside the namespace.
const INSIDE: &str = "SPOOLFRAME_TEST_PRIVATE_LAN";

/// Run `test` again in a new user and network namespace (`unshare -rn`)
/// with loopback up and taking multicast, and assert that it passed there.
/// Returns `true` in the copy inside, which then runs the test's body;
/// `false` in the outer one, which has nothing left to do.
fn private_lan(test: &str) -> bool {
    if std::env::var_os(INSIDE).is_some() {
        return true;
    }
    let exe = std::env::current_exe().unwrap();
    // libtest's name for it: this module's path in the binary, then `test`.
    let (_, module) = module_path!().split_once("::").unwrap();
    let test = format!("{module}::{test}");
    let output = std::process::Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "sh", "-ec"])
        .arg(
            "ip link set lo up; ip link set lo multicast on; \
             ip route add 224.0.0.0/4 dev lo; exec \"$0\" --exact \"$1\" --nocapture",
        )
        .arg(&exe)
        .arg(&test)
        .env(INSIDE, "1")
        .output()
        .expect("unshare (util-linux) runs");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{test} in a private LAN:\n{log}");
    assert!(log.contains("1 passed"), "{test} did not run:\n{log}");
    false
}

fn on_lan(name: &str) -> ServerOptions {
    ServerOptions {
        name: name.into(),
        mdns: true,
        ..ServerOptions::default()
    }
}

#[tokio::test]
async fn the_server_announces_itself_with_its_name() {
    if !private_lan("the_server_announces_itself_with_its_name") {
        return;
    }
    let listener = Listener::on(lan::MDNS_SERVICE).await;
    let server = TestServer::start_with(fresh_dir("announce"), on_lan("Den")).await;
    let data = listener.hear(&server.server.id()).await.unwrap();
    assert_eq!(lan::parse_advert(&data).unwrap().name, "Den");
    server.stop().await;
}

#[tokio::test]
async fn a_server_heard_on_the_lan_is_pinned_on_connect_and_stays_pinned() {
    if !private_lan("a_server_heard_on_the_lan_is_pinned_on_connect_and_stays_pinned") {
        return;
    }
    let server = TestServer::start_with(fresh_dir("pin"), on_lan("Den")).await;
    let storage = Arc::new(MemoryStorage::default());
    let client = open(&storage).await;
    let heard = entry(&client, &server, |e| e.on_network).await;
    assert_eq!(heard.name, "Den");
    assert!(!heard.pinned);

    let connected = client.connect(&heard.endpoint_id).await.unwrap();
    assert!(connected.pinned);
    client.close().await;

    // The app restarts: the same device, and the same pin.
    let again = open(&storage).await;
    assert_eq!(again.id(), client.id());
    let saved = again.servers().await.unwrap();
    let pinned: Vec<_> = saved.iter().filter(|e| e.pinned).collect();
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0].endpoint_id, server.server.id().to_string());
    again.close().await;
    server.stop().await;
}

#[tokio::test]
async fn another_server_with_the_same_name_is_a_different_server() {
    if !private_lan("another_server_with_the_same_name_is_a_different_server") {
        return;
    }
    let first = TestServer::start_with(fresh_dir("same-name-1"), on_lan("Home Media")).await;
    let storage = Arc::new(MemoryStorage::default());
    let client = open(&storage).await;
    let pinned = entry(&client, &first, |e| e.on_network).await;
    client.connect(&pinned.endpoint_id).await.unwrap();

    let impostor = TestServer::start_with(fresh_dir("same-name-2"), on_lan("Home Media")).await;
    let other = entry(&client, &impostor, |e| e.on_network).await;
    assert_eq!(other.name, "Home Media");
    assert!(!other.pinned);
    let servers = client.servers().await.unwrap();
    assert_eq!(servers.len(), 2);
    let still = servers.iter().find(|e| e.pinned).unwrap();
    assert_eq!(still.endpoint_id, pinned.endpoint_id);
    client.close().await;
    first.stop().await;
    impostor.stop().await;
}

#[tokio::test]
async fn endpoints_that_are_not_spoolframe_servers_are_not_listed() {
    if !private_lan("endpoints_that_are_not_spoolframe_servers_are_not_listed") {
        return;
    }
    let iroh_listener = Listener::on("irohv1").await;
    let ours = Listener::on(lan::MDNS_SERVICE).await;
    let client = open(&Arc::new(MemoryStorage::default())).await;
    let other_app = decoy("irohv1", Some(&lan::advert("Home Media"))).await;
    let no_advert = decoy(lan::MDNS_SERVICE, None).await;
    let other_protocol = decoy(lan::MDNS_SERVICE, Some("spoolframe/client/2;Home Media")).await;
    // Every decoy is on the air...
    iroh_listener.hear(&other_app.id()).await;
    ours.hear(&no_advert.id()).await;
    ours.hear(&other_protocol.id()).await;
    // ...and a real server, heard after them, is the only one listed.
    let server = TestServer::start_with(fresh_dir("decoys"), on_lan("Den")).await;
    entry(&client, &server, |e| e.on_network).await;
    let listed: Vec<_> = client
        .servers()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.endpoint_id)
        .collect();
    assert_eq!(listed, [server.server.id().to_string()]);
    for decoy in [other_app, no_advert, other_protocol] {
        decoy.close().await;
    }
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_pinned_server_on_a_new_address_is_found_again_by_its_key() {
    if !private_lan("a_pinned_server_on_a_new_address_is_found_again_by_its_key") {
        return;
    }
    let dir = fresh_dir("new-address");
    let server = TestServer::start_with(dir.clone(), on_lan("Den")).await;
    let id = server.server.id();
    let old_addrs: Vec<_> = server.server.addr().ip_addrs().copied().collect();
    let client = open(&Arc::new(MemoryStorage::default())).await;
    let heard = entry(&client, &server, |e| e.on_network).await;
    client.connect(&heard.endpoint_id).await.unwrap();

    // A new lease: the same server, its same key, another address.
    server.stop().await;
    let server = TestServer::start_with(dir, on_lan("Den")).await;
    assert_eq!(server.server.id(), id);
    let new_addrs: Vec<_> = server.server.addr().ip_addrs().copied().collect();
    assert!(new_addrs.iter().all(|a| !old_addrs.contains(a)));

    let found = until("connected at the new address", || async {
        client.connect(&heard.endpoint_id).await.ok()
    })
    .await;
    assert_eq!(found.endpoint_id, id.to_string());
    // The pin now holds where the server answered, ahead of the next record.
    let listed = client.servers().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].pinned);
    assert!(new_addrs.contains(&listed[0].addrs[0]), "{listed:?}");
    client.close().await;
    server.stop().await;
}

#[tokio::test]
async fn the_client_announces_nothing() {
    if !private_lan("the_client_announces_nothing") {
        return;
    }
    let ours = Listener::on(lan::MDNS_SERVICE).await;
    let iroh_default = Listener::on("irohv1").await;
    let server = TestServer::start_with(fresh_dir("silent"), on_lan("Den")).await;
    let client = open(&Arc::new(MemoryStorage::default())).await;
    let heard = entry(&client, &server, |e| e.on_network).await;
    client.connect(&heard.endpoint_id).await.unwrap();
    // The listeners work: they hear the server...
    ours.hear(&server.server.id()).await;
    // ...and, given time to, never the client.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let client_id: EndpointId = client.id().parse().unwrap();
    assert_eq!(ours.get(&client_id), None);
    assert_eq!(iroh_default.get(&client_id), None);
    client.close().await;
    server.stop().await;
}
