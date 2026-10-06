//! LAN discovery over mDNS (P1-13).
//!
//! Each test runs itself again inside a private network namespace
//! ([`private_lan`]) whose only interface is loopback, with multicast on.
//! There, every record is from this test: no other Spoolframe server on the
//! real LAN is heard, and the host firewall, which usually drops inbound
//! mDNS (UDP 5353), is not in the way.
//!
//! The assertions still name the servers they look for by key, never by
//! how many are listed: a test says what it knows about its own servers,
//! whatever else may be heard. The other integration tests do not listen
//! on the LAN at all ([`support::sdk_client`]).

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
use spoolframe_client_sdk::{Client, ClientOptions, Device, MemoryStorage, SdkError, ServerEntry};
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
        platform: spoolframe_client_sdk::types::Platform::Cli,
        capabilities: ClientCapabilities::default(),
    }
}

async fn open(storage: &Arc<MemoryStorage>) -> Client {
    Client::open(storage.clone(), device()).await.unwrap()
}

/// The keys `client` lists, of those in `ours`.
async fn listed_of(client: &Client, ours: &[&str]) -> Vec<String> {
    client
        .servers()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.endpoint_id)
        .filter(|id| ours.contains(&id.as_str()))
        .collect()
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
    let server = TestServer::start_with(fresh_dir("announce").await, on_lan("Den")).await;
    let data = listener.hear(&server.server.id()).await.unwrap();
    assert_eq!(lan::parse_advert(&data).unwrap().name, "Den");
    server.stop().await;
}

#[tokio::test]
async fn a_server_is_saved_only_when_pinned_and_stays_pinned() {
    if !private_lan("a_server_is_saved_only_when_pinned_and_stays_pinned") {
        return;
    }
    let server = TestServer::start_with(fresh_dir("pin").await, on_lan("Den")).await;
    let storage = Arc::new(MemoryStorage::default());
    let client = open(&storage).await;
    let heard = entry(&client, &server, |e| e.on_network).await;
    assert_eq!(heard.name, "Den");
    assert!(!heard.pinned);

    // Connecting shows who answered, and saves nothing (P1-21).
    let connected = client.connect(&heard.endpoint_id).await.unwrap();
    assert!(!connected.pinned);
    assert_eq!(
        Some(connected.fingerprint.clone()),
        spoolframe_protocol::fingerprint::of_hex(&server.server.id().to_string())
    );
    assert!(matches!(
        client.profiles(&heard.endpoint_id).await,
        Err(SdkError::NotPinned(_))
    ));

    // "Skip": saved, not verified; "Verify now": verified, same pin.
    let pinned = client.pin(&heard.endpoint_id, false).await.unwrap();
    assert!(pinned.pinned && !pinned.verified, "{pinned:?}");
    let verified = client.pin(&heard.endpoint_id, true).await.unwrap();
    assert!(verified.pinned && verified.verified, "{verified:?}");
    client.close().await;

    // The app restarts: the same device, and the same pin, still verified.
    let again = open(&storage).await;
    assert_eq!(again.id(), client.id());
    let saved = again.servers().await.unwrap();
    let pinned: Vec<_> = saved.iter().filter(|e| e.pinned).collect();
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0].endpoint_id, server.server.id().to_string());
    assert!(pinned[0].verified);
    again.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_server_connected_but_not_pinned_is_forgotten() {
    if !private_lan("a_server_connected_but_not_pinned_is_forgotten") {
        return;
    }
    let server = TestServer::start_with(fresh_dir("cancel").await, on_lan("Den")).await;
    let storage = Arc::new(MemoryStorage::default());
    let client = open(&storage).await;
    let heard = entry(&client, &server, |e| e.on_network).await;
    client.connect(&heard.endpoint_id).await.unwrap();
    client.close().await;
    // "Cancel": nothing was saved, so nothing is listed once the server
    // is not heard.
    let again = open(&storage).await;
    server.stop().await;
    assert!(again.servers().await.unwrap().iter().all(|e| !e.pinned));
    again.close().await;
}

#[tokio::test]
async fn another_server_with_the_same_name_beside_it_is_a_different_server() {
    if !private_lan("another_server_with_the_same_name_beside_it_is_a_different_server") {
        return;
    }
    let first = TestServer::start_with(fresh_dir("same-name-1").await, on_lan("Home Media")).await;
    let storage = Arc::new(MemoryStorage::default());
    let client = open(&storage).await;
    let pinned = entry(&client, &first, |e| e.on_network).await;
    client.connect(&pinned.endpoint_id).await.unwrap();
    client.pin(&pinned.endpoint_id, true).await.unwrap();

    let impostor =
        TestServer::start_with(fresh_dir("same-name-2").await, on_lan("Home Media")).await;
    let other = entry(&client, &impostor, |e| e.on_network).await;
    assert_eq!(other.name, "Home Media");
    assert!(!other.pinned);
    assert_ne!(other.fingerprint, pinned.fingerprint);
    let mut listed = listed_of(&client, &[&pinned.endpoint_id, &other.endpoint_id]).await;
    listed.sort();
    let mut both = [pinned.endpoint_id.clone(), other.endpoint_id.clone()];
    both.sort();
    assert_eq!(listed, both);
    let servers = client.servers().await.unwrap();
    let still: Vec<_> = servers.iter().filter(|e| e.pinned).collect();
    assert_eq!(still.len(), 1, "{servers:?}");
    assert_eq!(still[0].endpoint_id, pinned.endpoint_id);
    // Both are on the air: the second is another server, not a new key.
    assert!(!client.connect(&other.endpoint_id).await.unwrap().pinned);
    client.close().await;
    first.stop().await;
    impostor.stop().await;
}

/// The saved server is gone and another key answers with its server id
/// (P1-13, PX-14): the server's key was replaced. Nothing connects or saves
/// on its own; the user's Cancel keeps the pin, Trust replaces it and
/// keeps the server id.
#[tokio::test]
async fn a_new_key_with_a_saved_server_id_is_an_identity_change() {
    if !private_lan("a_new_key_with_a_saved_server_id_is_an_identity_change") {
        return;
    }
    let first = TestServer::start_with(fresh_dir("rekey-1").await, on_lan("Home Media")).await;
    first.add_user("alice", "correct horse", None).await;
    let storage = Arc::new(MemoryStorage::default());
    let client = open(&storage).await;
    let old = entry(&client, &first, |e| e.on_network).await.endpoint_id;
    client.connect(&old).await.unwrap();
    client.pin(&old, true).await.unwrap();
    client
        .sign_in(&old, "alice", "correct horse", false)
        .await
        .unwrap();

    let server_id = first.server_id.clone();
    first.stop().await;
    let dir = fresh_dir("rekey-2").await;
    std::fs::create_dir_all(&*dir).unwrap();
    std::fs::write(
        dir.join(spoolframe_server::identity::SERVER_ID_FILE),
        server_id.as_str(),
    )
    .unwrap();
    let second = TestServer::start_with(dir, on_lan("Home Media")).await;
    let new = second.server.id().to_string();
    let changed = SdkError::IdentityChanged {
        saved: old.clone(),
        now: new.clone(),
    };
    // Once it is heard, picking it says so, rather than connecting.
    until("the new key is an identity change", || async {
        (client.connect(&new).await == Err(changed.clone())).then_some(())
    })
    .await;
    // A pin is not taken over by pinning.
    assert_eq!(client.pin(&new, true).await, Err(changed.clone()));
    // Cancel: nothing changed.
    let pinned: Vec<_> = client
        .servers()
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.pinned)
        .map(|e| e.endpoint_id)
        .collect();
    assert_eq!(pinned, std::slice::from_ref(&old));

    // Trust: the new key is the pin, verified, with the same server id,
    // and the old sessions are gone with the old key.
    let trusted = client.trust_new_identity(&old, &new).await.unwrap();
    assert!(trusted.pinned && trusted.verified, "{trusted:?}");
    // The old key may still be heard a while: it is listed, not pinned.
    let pinned: Vec<_> = client
        .servers()
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.pinned)
        .map(|e| (e.endpoint_id, e.server_id))
        .collect();
    assert_eq!(pinned, [(new.clone(), Some(server_id.to_string()))]);
    assert!(client.profiles(&new).await.unwrap().is_empty());
    assert!(matches!(
        client.profiles(&old).await,
        Err(SdkError::NotPinned(_))
    ));
    client.close().await;
    second.stop().await;
}

/// The saved server is gone and another key, with another server id,
/// answers under its name: another server (PX-14). A name says nothing:
/// it is listed apart, connects as a server not pinned, and the saved one
/// stays as it was.
#[tokio::test]
async fn a_new_key_and_server_id_under_a_saved_name_is_another_server() {
    if !private_lan("a_new_key_and_server_id_under_a_saved_name_is_another_server") {
        return;
    }
    let first = TestServer::start_with(fresh_dir("reinstall-1").await, on_lan("Home Media")).await;
    let storage = Arc::new(MemoryStorage::default());
    let client = open(&storage).await;
    let old = entry(&client, &first, |e| e.on_network).await.endpoint_id;
    client.connect(&old).await.unwrap();
    client.pin(&old, true).await.unwrap();

    first.stop().await;
    let second = TestServer::start_with(fresh_dir("reinstall-2").await, on_lan("Home Media")).await;
    let other = entry(&client, &second, |e| e.on_network).await;
    assert!(!other.pinned, "{other:?}");
    assert!(!client.connect(&other.endpoint_id).await.unwrap().pinned);
    let listed = client.servers().await.unwrap();
    let ours: Vec<_> = listed
        .iter()
        .filter(|e| e.endpoint_id == old || e.endpoint_id == other.endpoint_id)
        .collect();
    assert_eq!(ours.len(), 2, "{listed:?}");
    assert!(listed.iter().any(|e| e.pinned && e.endpoint_id == old));
    client.close().await;
    second.stop().await;
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
    let server = TestServer::start_with(fresh_dir("decoys").await, on_lan("Den")).await;
    entry(&client, &server, |e| e.on_network).await;
    let decoys = [
        other_app.id().to_string(),
        no_advert.id().to_string(),
        other_protocol.id().to_string(),
    ];
    let decoys: Vec<&str> = decoys.iter().map(String::as_str).collect();
    assert_eq!(listed_of(&client, &decoys).await, Vec::<String>::new());
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
    let dir = fresh_dir("new-address").await;
    let server = TestServer::start_with(dir.clone(), on_lan("Den")).await;
    let id = server.server.id();
    let old_addrs: Vec<_> = server.server.addr().ip_addrs().copied().collect();
    let client = open(&Arc::new(MemoryStorage::default())).await;
    let heard = entry(&client, &server, |e| e.on_network).await;
    client.connect(&heard.endpoint_id).await.unwrap();
    client.pin(&heard.endpoint_id, true).await.unwrap();

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
    let [ours] = &listed
        .iter()
        .filter(|e| e.endpoint_id == id.to_string())
        .collect::<Vec<_>>()[..]
    else {
        panic!("not one entry for {id}: {listed:?}");
    };
    assert!(ours.pinned);
    assert!(new_addrs.contains(&ours.addrs[0]), "{listed:?}");
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
    let server = TestServer::start_with(fresh_dir("silent").await, on_lan("Den")).await;
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

#[tokio::test]
async fn the_server_list_tells_when_a_server_is_heard() {
    if !private_lan("the_server_list_tells_when_a_server_is_heard") {
        return;
    }
    let client = open(&Arc::new(MemoryStorage::default())).await;
    // An app lists at once: nothing pinned, and nothing heard yet in this
    // private namespace.
    let mut changes = client.watch_servers();
    assert!(client.servers().await.unwrap().is_empty());

    let server = TestServer::start_with(fresh_dir("watch").await, on_lan("Den")).await;
    let id = server.server.id().to_string();
    // No polling: each change is the cue to list again.
    let listed = tokio::time::timeout(HEAR_WITHIN, async {
        loop {
            assert!(changes.changed().await, "the client went away");
            if let Some(entry) = client
                .servers()
                .await
                .unwrap()
                .into_iter()
                .find(|e| e.endpoint_id == id)
            {
                return entry;
            }
        }
    })
    .await
    .expect("no change listed the server");
    assert_eq!(listed.name, "Den");
    client.close().await;
    server.stop().await;
}

/// A client opened with LAN discovery off (PX-10) never lists or dials a
/// server it only could have heard: the switch the other tests use to keep
/// a developer's own server out of their results.
#[tokio::test]
async fn a_client_with_discovery_off_hears_no_server() {
    if !private_lan("a_client_with_discovery_off_hears_no_server") {
        return;
    }
    let server = TestServer::start_with(fresh_dir("deaf").await, on_lan("Den")).await;
    let deaf = Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        device(),
        ClientOptions {
            lan_discovery: false,
            ..ClientOptions::default()
        },
    )
    .await
    .unwrap();
    // A listening client hears the server: it is on the air...
    let listening = open(&Arc::new(MemoryStorage::default())).await;
    let heard = entry(&listening, &server, |e| e.on_network).await;
    // ...and given as long again, the deaf one lists nothing and cannot
    // dial a key it was given no address for.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(deaf.servers().await.unwrap().is_empty());
    assert!(matches!(
        deaf.connect(&heard.endpoint_id).await,
        Err(SdkError::UnknownServer(_))
    ));
    deaf.close().await;
    listening.close().await;
    server.stop().await;
}
