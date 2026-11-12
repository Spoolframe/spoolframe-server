//! Adding a server by its address alone: the server answers
//! `/.well-known/spoolframe` on its web listener with what an invite link
//! says of it, and a device that typed the address looks the key up
//! there, connects and pins it. The listener here is plain HTTP on
//! loopback, so the device is let look up `http://` (a test's only).

use std::net::SocketAddr;
use std::sync::Arc;

use spoolframe_client_sdk::{MemoryStorage, SdkError};
use spoolframe_protocol::fingerprint;
use spoolframe_server::admin::{Admin, LookupDocument, Origins, StatusSource, TrustedProxies};

use crate::relays::{device_on, reaches, relay, server_on};
use crate::support::{self, ServerOptions, TestServer};

const PUBLIC_URL: &str = "https://media.example.org";

/// `server`'s web listener on `listen`, with `origins` and `document`.
async fn web(
    server: &TestServer,
    listen: SocketAddr,
    origins: Origins,
    document: LookupDocument,
) -> (SocketAddr, jsonrpsee::server::ServerHandle) {
    let status = StatusSource::new(
        server.name.clone(),
        server.server_id.clone(),
        support::transcoding(),
    )
    .with_lookup_document(document);
    status.set_endpoint_id(server.server.id().to_string());
    status.set_told(server.server.told());
    spoolframe_server::admin::serve(
        listen,
        Admin {
            status: Arc::new(status),
            ..crate::admin::admin(server.auth.clone(), None)
        },
        origins,
        TrustedProxies::default(),
    )
    .await
    .unwrap()
}

/// The web listener of a server with a public URL, the document on.
async fn published(server: &TestServer) -> (SocketAddr, jsonrpsee::server::ServerHandle) {
    web(
        server,
        "127.0.0.1:0".parse().unwrap(),
        Origins::Exactly(PUBLIC_URL.to_owned()),
        LookupDocument::Public,
    )
    .await
}

/// What `server` tells a signed-in device of its addresses, kept by
/// `keep`, as the document writes them.
fn told_kept(server: &TestServer, keep: impl Fn(&SocketAddr) -> bool) -> serde_json::Value {
    let addrs: Vec<String> = server
        .server
        .told()
        .addrs()
        .into_iter()
        .filter(|a| keep(a))
        .map(|a| a.to_string())
        .collect();
    serde_json::json!(addrs)
}

fn public(addr: &SocketAddr) -> bool {
    spoolframe_admin_api::wan::not_public(addr.ip()).is_none()
}

async fn get(addr: SocketAddr) -> reqwest::Response {
    reqwest::get(format!("http://{addr}/.well-known/spoolframe"))
        .await
        .unwrap()
}

#[tokio::test]
async fn the_document_says_what_an_invite_link_says_and_may_be_kept_a_while() {
    let (relay, url) = relay().await;
    let server = server_on("lookup-document", &url).await;
    let (addr, _web) = published(&server).await;
    let answer = get(addr).await;
    assert_eq!(answer.status(), 200);
    assert_eq!(
        answer.headers()["cache-control"].to_str().unwrap(),
        "public, max-age=300"
    );
    let doc: serde_json::Value = answer.json().await.unwrap();
    assert_eq!(
        doc,
        serde_json::json!({
            "endpoint_id": server.server.id().to_string(),
            "name": "Home Media",
            // Its public addresses only: never loopback or a LAN's.
            "addrs": told_kept(&server, public),
            "relays": [url.to_string()],
            "lookup": null,
        })
    );
    server.stop().await;
    drop(relay);
}

#[tokio::test]
async fn without_a_public_url_there_is_no_document() {
    let server = TestServer::start("lookup-no-public-url").await;
    let (addr, _web) = web(
        &server,
        "127.0.0.1:0".parse().unwrap(),
        Origins::SameHost,
        LookupDocument::Public,
    )
    .await;
    assert_eq!(get(addr).await.status(), 404);
    server.stop().await;
}

#[tokio::test]
async fn with_publish_lookup_off_there_is_no_document() {
    let server = TestServer::start("lookup-off").await;
    let (addr, _web) = web(
        &server,
        "127.0.0.1:0".parse().unwrap(),
        Origins::Exactly(PUBLIC_URL.to_owned()),
        LookupDocument::Off,
    )
    .await;
    assert_eq!(get(addr).await.status(), 404);
    server.stop().await;
}

/// By default the document lists public addresses only; with the
/// private ones on, the home network's, CGNAT's and every other told one
/// too, but never loopback.
#[tokio::test]
async fn private_addresses_are_listed_only_when_asked_and_loopback_never() {
    use spoolframe_server::announce::{Added, Announce};
    let added = [
        "127.0.0.1:7420",
        "192.168.1.10:7420",
        "100.64.1.1:7420",
        "8.8.8.8:7420",
    ];
    let options = ServerOptions {
        announce: Announce {
            add: added
                .iter()
                .map(|a| Added::Socket(a.parse().unwrap()))
                .collect(),
            ..Announce::default()
        },
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(support::fresh_dir("lookup-private").await, options).await;
    let told = server.server.told().addrs();
    for a in added {
        assert!(told.contains(&a.parse().unwrap()), "{a} not told: {told:?}");
    }
    let listed = |doc: &serde_json::Value| -> Vec<String> {
        serde_json::from_value(doc["addrs"].clone()).unwrap()
    };
    let origins = || Origins::Exactly(PUBLIC_URL.to_owned());
    let any = || "127.0.0.1:0".parse().unwrap();

    let (public_only, _a) = web(&server, any(), origins(), LookupDocument::Public).await;
    let doc: serde_json::Value = get(public_only).await.json().await.unwrap();
    assert_eq!(doc["addrs"], told_kept(&server, public));
    assert!(listed(&doc).contains(&"8.8.8.8:7420".to_owned()));
    for private in ["127.0.0.1:7420", "192.168.1.10:7420", "100.64.1.1:7420"] {
        assert!(!listed(&doc).contains(&private.to_owned()), "{private}");
    }

    let (with_private, _b) = web(&server, any(), origins(), LookupDocument::WithPrivate).await;
    let doc: serde_json::Value = get(with_private).await.json().await.unwrap();
    assert_eq!(doc["addrs"], told_kept(&server, |a| !a.ip().is_loopback()));
    for listed_too in ["192.168.1.10:7420", "100.64.1.1:7420", "8.8.8.8:7420"] {
        assert!(
            listed(&doc).contains(&listed_too.to_owned()),
            "{listed_too}"
        );
    }
    assert!(!listed(&doc).contains(&"127.0.0.1:7420".to_owned()));
    server.stop().await;
}

/// The whole flow, with the device's relays off: the address typed, the
/// key looked up, connected and pinned. The document named a relay, and
/// pinning chose it for the server, so a device opened again reaches
/// it through the relay alone.
#[tokio::test]
async fn a_server_found_by_its_address_is_connected_pinned_and_reached_through_its_relays() {
    let (relay, url) = relay().await;
    let server = server_on("lookup-flow", &url).await;
    let (addr, _web) = published(&server).await;
    let id = server.server.id().to_string();
    let storage = Arc::new(MemoryStorage::default());
    let client = device_on(false, storage.clone()).await;
    client.allow_http_lookups();

    let found = client
        .look_up_server(&format!(" http://{addr}/anything "))
        .await
        .unwrap();
    assert_eq!(
        (found.endpoint_id.as_str(), found.name.as_str()),
        (id.as_str(), "Home Media")
    );
    assert_eq!(found.fingerprint, fingerprint::of_hex(&id).unwrap());
    assert_eq!(found.relays, [url.to_string()]);
    assert!(found.addrs.iter().all(public), "{:?}", found.addrs);
    // Nothing saved by the lookup.
    assert!(client.servers().await.unwrap().iter().all(|s| !s.pinned));

    let entry = client.connect(&id).await.unwrap();
    assert_eq!(entry.fingerprint, found.fingerprint);
    assert!(!entry.pinned);
    let pinned = client.pin(&id, true).await.unwrap();
    assert!(pinned.pinned && pinned.verified);
    assert_eq!(pinned.use_relays, Some(true));
    assert!(pinned.relays.contains(&url.to_string()), "{pinned:?}");
    client.close().await;

    assert!(reaches(storage, false, &server).await);
    server.stop().await;
    drop(relay);
}

/// The address a saved server was looked up at now names another key:
/// `IdentityChanged` with both fingerprints, before anything is dialled.
/// Nothing is approved until the user says so; then
/// `trust_new_identity` reaches the new key through what the document
/// named.
#[tokio::test]
async fn another_key_at_the_address_is_an_identity_change_the_user_approves() {
    let (relay, url) = relay().await;
    let old = server_on("lookup-old-key", &url).await;
    let new = server_on("lookup-new-key", &url).await;
    let old_id = old.server.id().to_string();
    let new_id = new.server.id().to_string();
    let (addr, web_old) = published(&old).await;
    let storage = Arc::new(MemoryStorage::default());
    let client = device_on(false, storage.clone()).await;
    client.allow_http_lookups();
    let typed = format!("http://{addr}");
    client.look_up_server(&typed).await.unwrap();
    client.connect(&old_id).await.unwrap();
    client.pin(&old_id, true).await.unwrap();
    client.close().await;

    // The same address now answers with the other server's document.
    web_old.stop().unwrap();
    web_old.stopped().await;
    let (_, _web_new) = web(
        &new,
        addr,
        Origins::Exactly(PUBLIC_URL.to_owned()),
        LookupDocument::Public,
    )
    .await;

    let client = device_on(false, storage.clone()).await;
    client.allow_http_lookups();
    let changed = client.look_up_server(&typed).await.unwrap_err();
    assert_eq!(
        changed,
        SdkError::IdentityChanged {
            saved: old_id.clone(),
            now: new_id.clone(),
            server_id: Some(old.server_id.to_string())
        }
    );
    let prints = (
        fingerprint::of_hex(&old_id).unwrap(),
        fingerprint::of_hex(&new_id).unwrap(),
    );
    assert_eq!(changed.fingerprints(), Some(prints.clone()));
    // As the apps see it: both keys and both fingerprints.
    assert_eq!(
        spoolframe_client_ffi::SpoolError::from(changed),
        spoolframe_client_ffi::SpoolError::IdentityChanged {
            saved: old_id.clone(),
            now: new_id.clone(),
            server_id: Some(old.server_id.to_string()),
            saved_fingerprint: prints.0,
            now_fingerprint: prints.1,
        }
    );
    // Not approved by the lookup.
    let saved = client.servers().await.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].endpoint_id, old_id);

    let trusted = client.trust_new_identity(&old_id, &new_id).await.unwrap();
    assert_eq!(trusted.endpoint_id, new_id);
    assert!(trusted.pinned);
    assert_eq!(trusted.use_relays, Some(true));
    // The address is the new key's now: looked up again, no change.
    let found = client.look_up_server(&typed).await.unwrap();
    assert_eq!(found.endpoint_id, new_id);
    client.close().await;
    old.stop().await;
    new.stop().await;
    drop(relay);
}

/// The user's own addresses for a saved server are kept with it and
/// dialled: with every other address forgotten and no relays, they
/// alone reach it. Empty clears them; an unsaved server is refused.
#[tokio::test]
async fn a_saved_server_s_own_addresses_are_kept_and_dialled() {
    let server = TestServer::start("lookup-own-addrs").await;
    let id = server.server.id().to_string();
    let addrs: Vec<SocketAddr> = server.server.addr().ip_addrs().copied().collect();
    let storage = Arc::new(MemoryStorage::default());
    let client = device_on(false, storage.clone()).await;
    assert_eq!(
        client.set_server_addresses(&id, addrs.clone()).await,
        Err(SdkError::NotPinned(id.clone()))
    );
    client.add_address(&id, addrs.clone()).unwrap();
    client.connect(&id).await.unwrap();
    client.pin(&id, true).await.unwrap();
    client
        .set_server_addresses(&id, addrs.clone())
        .await
        .unwrap();
    assert_eq!(client.servers().await.unwrap()[0].user_addrs, addrs);
    client.close().await;
    // `reaches` forgets the addresses saved and told: not the user's.
    assert!(reaches(storage.clone(), false, &server).await);

    let client = device_on(false, storage.clone()).await;
    client.set_server_addresses(&id, Vec::new()).await.unwrap();
    assert!(client.servers().await.unwrap()[0].user_addrs.is_empty());
    client.close().await;
    assert!(!reaches(storage, false, &server).await);
    server.stop().await;
}
