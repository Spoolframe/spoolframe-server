//! One search screen for every server: with federation off, a search for
//! hits is the server's own search, as hits of its own items. And the
//! catalog cache, which answers a read again without the server.

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Platform, SearchScope};
use spoolframe_client_sdk::{
    CacheConfig, Client, ClientOptions, Device, MemoryStorage, SearchRequest, Session, Timeouts,
};

use crate::stub::Stub;

async fn signed_in(stub: &Stub, catalog_cache: Option<CacheConfig>) -> (Client, Session) {
    let client = Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        Device {
            name: "phone".into(),
            client_name: "sdk-search-test".into(),
            client_version: "0".into(),
            platform: Platform::Ios,
            capabilities: ClientCapabilities::default(),
        },
        ClientOptions {
            timeouts: Timeouts {
                request: Duration::from_secs(5),
                ping: Duration::from_secs(1),
            },
            lan_discovery: false,
            catalog_cache,
            ..ClientOptions::default()
        },
    )
    .await
    .unwrap();
    client.add_address(&stub.id(), stub.addrs()).unwrap();
    client.connect(&stub.id()).await.unwrap();
    client.pin(&stub.id(), true).await.unwrap();
    let session = client
        .sign_in(&stub.id(), "alice", "correct horse", false)
        .await
        .unwrap();
    (client, session)
}

#[tokio::test]
async fn hits_without_federation_are_the_servers_own_search() {
    let stub = Stub::start().await;
    let (client, session) = signed_in(&stub, None).await;
    assert!(!session.experimental().federation);
    let hits = session
        .search_hits(SearchRequest {
            query: "alien".into(),
            limit: 10,
            scope: SearchScope::All,
            kind: None,
            library: None,
        })
        .await
        .unwrap();
    assert!(hits.groups.is_empty());
    assert_eq!(stub.count("search"), 1);
    client.close().await;
    stub.stop().await;
}

#[tokio::test]
async fn a_kept_read_is_answered_without_the_server_until_forgotten() {
    let stub = Stub::start().await;
    let (client, session) = signed_in(&stub, Some(CacheConfig::default())).await;
    session.libraries().await.unwrap();
    session.libraries().await.unwrap();
    assert_eq!(stub.count("libraries"), 1);
    session.forget_catalog();
    session.libraries().await.unwrap();
    assert_eq!(stub.count("libraries"), 2);
    client.close().await;
    stub.stop().await;
}

/// Another device marked an item watched: Home, kept with it, is read
/// from the server again, not answered with the old state.
#[tokio::test]
async fn a_watch_change_elsewhere_forgets_the_kept_home() {
    let stub = Stub::start().await;
    let (client, session) = signed_in(&stub, Some(CacheConfig::default())).await;
    session.home().await.unwrap();
    session.home().await.unwrap();
    assert_eq!(stub.count("home"), 1);
    let item = "plex:movie:1".parse().unwrap();
    let mut told = client.watch_events();
    stub.push_watch_changed(vec![item]).await;
    tokio::time::timeout(Duration::from_secs(5), told.recv())
        .await
        .unwrap()
        .unwrap();
    session.home().await.unwrap();
    assert_eq!(stub.count("home"), 2);
    client.close().await;
    stub.stop().await;
}

#[tokio::test]
async fn with_the_cache_off_every_read_asks_the_server() {
    let stub = Stub::start().await;
    let (client, session) = signed_in(&stub, None).await;
    session.libraries().await.unwrap();
    session.libraries().await.unwrap();
    assert_eq!(stub.count("libraries"), 2);
    client.close().await;
    stub.stop().await;
}
