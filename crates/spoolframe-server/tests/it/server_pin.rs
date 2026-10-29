//! The SDK's pin is the pair of the server's key and its server id
//! (PX-14). A connect that is not asked about needs both to match what was
//! pinned. The name may change freely. A new key with the same server id
//! is the server with its key replaced: an identity change, which the
//! user accepts or not. A new server id under the same key is refused:
//! a server never changes its id. A new key and a new id are another
//! server.
//!
//! The servers here are off the LAN: a client is given their addresses.
//! The client does not listen on the LAN either ([`crate::support::sdk_client`]),
//! so `Client::servers` lists what the test saved and nothing a server on
//! the developer's network announces.

use std::sync::Arc;

use spoolframe_client_sdk::{Client, Device, MemoryStorage, SdkError, ServerEntry};
use spoolframe_server::identity::SERVER_ID_FILE;

use crate::support::{ServerOptions, StateDir, TestServer, fresh_dir, mkv_capabilities};

fn device() -> Device {
    Device {
        name: "living room".into(),
        client_name: "sdk-test".into(),
        client_version: "0".into(),
        platform: spoolframe_client_sdk::types::Platform::Tvos,
        capabilities: mkv_capabilities(),
    }
}

fn id(server: &TestServer) -> String {
    server.server.id().to_string()
}

/// A client on `storage` that knows where each of `servers` is.
async fn client_of(storage: &Arc<MemoryStorage>, servers: &[&TestServer]) -> Client {
    let client = crate::support::sdk_client(storage.clone(), device())
        .await
        .unwrap();
    for server in servers {
        let addrs = server.server.addr().ip_addrs().copied().collect();
        client.add_address(&id(server), addrs).unwrap();
    }
    client
}

/// Connect to `server` and pin it, as an app does once the user checked
/// the fingerprint.
async fn pin(client: &Client, server: &TestServer) -> ServerEntry {
    client.connect(&id(server)).await.unwrap();
    client.pin(&id(server), true).await.unwrap()
}

/// A fresh state directory holding `server`'s server id, so a server
/// started on it has a new key and the same id.
async fn dir_with_the_id_of(test: &str, server: &TestServer) -> StateDir {
    let dir = fresh_dir(test).await;
    std::fs::create_dir_all(&*dir).unwrap();
    std::fs::write(dir.join(SERVER_ID_FILE), server.server_id.as_str()).unwrap();
    dir
}

#[tokio::test]
async fn the_first_connect_pins_the_key_and_the_server_id() {
    let server = TestServer::start("pin-both").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = client_of(&storage, &[&server]).await;
    let connected = client.connect(&id(&server)).await.unwrap();
    assert_eq!(
        connected.server_id.as_deref(),
        Some(server.server_id.as_str())
    );
    pin(&client, &server).await;
    let [saved] = &client.servers().await.unwrap()[..] else {
        panic!("not one server");
    };
    assert_eq!(
        (saved.endpoint_id.as_str(), saved.server_id.as_deref()),
        (id(&server).as_str(), Some(server.server_id.as_str()))
    );
    client.close().await;
    server.stop().await;
}

/// A rename is the server's to make: the saved name follows, and nothing
/// is asked.
#[tokio::test]
async fn a_new_name_is_taken_without_asking() {
    let server = TestServer::start("pin-rename").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = client_of(&storage, &[&server]).await;
    pin(&client, &server).await;
    client.close().await;

    server.name.set("Den");
    let client = client_of(&storage, &[&server]).await;
    let entry = client.connect(&id(&server)).await.unwrap();
    assert_eq!(
        (entry.name.as_str(), entry.pinned, entry.verified),
        ("Den", true, true)
    );
    let [saved] = &client.servers().await.unwrap()[..] else {
        panic!("not one server");
    };
    assert_eq!(saved.name, "Den");
    client.close().await;
    server.stop().await;
}

/// The same server id under a new key is the server with its key
/// replaced: the user is asked, and trusting the new key keeps the id,
/// so the server's app passkeys stay usable.
#[tokio::test]
async fn a_new_key_with_the_same_server_id_is_an_identity_change() {
    let old = TestServer::start("pin-new-key-old").await;
    let new = TestServer::start_in(dir_with_the_id_of("pin-new-key-new", &old).await).await;
    assert_eq!(new.server_id, old.server_id);
    let storage = Arc::new(MemoryStorage::default());
    let client = client_of(&storage, &[&old, &new]).await;
    pin(&client, &old).await;

    let changed = SdkError::IdentityChanged {
        saved: id(&old),
        now: id(&new),
        server_id: Some(old.server_id.to_string()),
    };
    assert_eq!(client.connect(&id(&new)).await, Err(changed.clone()));
    // A pin is not taken over by pinning.
    assert_eq!(client.pin(&id(&new), true).await, Err(changed));
    assert_eq!(client.servers().await.unwrap().len(), 1);

    let trusted = client
        .trust_new_identity(&id(&old), &id(&new))
        .await
        .unwrap();
    assert!(trusted.pinned && trusted.verified, "{trusted:?}");
    let [saved] = &client.servers().await.unwrap()[..] else {
        panic!("not one server");
    };
    assert_eq!(
        (saved.endpoint_id.as_str(), saved.server_id.as_deref()),
        (id(&new).as_str(), Some(old.server_id.as_str()))
    );
    client.close().await;
    old.stop().await;
    new.stop().await;
}

/// A server never changes its id: one that answers under its pinned key
/// with another is refused, and nothing it says is kept.
#[tokio::test]
async fn a_new_server_id_under_the_same_key_is_refused() {
    let server = TestServer::start("pin-new-id").await;
    let pinned_id = server.server_id.clone();
    let storage = Arc::new(MemoryStorage::default());
    let client = client_of(&storage, &[&server]).await;
    pin(&client, &server).await;
    client.close().await;

    let dir = server.dir.clone();
    server.stop().await;
    std::fs::write(dir.join(SERVER_ID_FILE), "aaaaaaaaaaaaaaaa").unwrap();
    // Any free port: the new client is given the address below. The old
    // port may be taken by now, by any endpoint started meanwhile.
    let server = TestServer::start_with(
        dir,
        ServerOptions {
            name: "Somewhere else".into(),
            ..ServerOptions::default()
        },
    )
    .await;

    let client = client_of(&storage, &[&server]).await;
    assert_eq!(
        client.connect(&id(&server)).await,
        Err(SdkError::ServerIdChanged {
            endpoint_id: id(&server),
            saved: pinned_id.to_string(),
            now: "aaaaaaaaaaaaaaaa".into(),
        })
    );
    assert!(matches!(
        client.sign_in(&id(&server), "alice", "x", false).await,
        Err(SdkError::ServerIdChanged { .. })
    ));
    let [saved] = &client.servers().await.unwrap()[..] else {
        panic!("not one server");
    };
    assert_eq!(
        (saved.server_id.as_deref(), saved.name.as_str()),
        (Some(pinned_id.as_str()), "Home Media")
    );
    client.close().await;
    server.stop().await;
}

/// A new key and a new id are another server, whatever it is called: no
/// pin applies to it, and it is saved beside the first if the user says.
#[tokio::test]
async fn a_new_key_and_a_new_server_id_are_another_server() {
    let first = TestServer::start("pin-other-1").await;
    let other = TestServer::start("pin-other-2").await;
    assert_eq!(first.name.get(), other.name.get());
    let storage = Arc::new(MemoryStorage::default());
    let client = client_of(&storage, &[&first, &other]).await;
    pin(&client, &first).await;

    let entry = client.connect(&id(&other)).await.unwrap();
    assert!(!entry.pinned, "{entry:?}");
    pin(&client, &other).await;
    assert_eq!(client.servers().await.unwrap().len(), 2);
    client.close().await;
    first.stop().await;
    other.stop().await;
}
