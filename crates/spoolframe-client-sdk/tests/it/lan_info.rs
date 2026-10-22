//! What a "servers on my network" row shows of a server, before signing
//! in: its `Hello` answer, with the fingerprint the apps show. Nothing is
//! saved by asking.

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{Client, ClientOptions, Device, MemoryStorage, SdkError, Timeouts};
use spoolframe_protocol::PROTOCOL_VERSION;

use crate::stub::Stub;

fn device() -> Device {
    Device {
        name: "phone".into(),
        client_name: "sdk-lan-info-test".into(),
        client_version: "0".into(),
        platform: Platform::Ios,
        capabilities: ClientCapabilities::default(),
    }
}

async fn client() -> Client {
    Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        device(),
        ClientOptions {
            timeouts: Timeouts {
                request: Duration::from_secs(5),
                ping: Duration::from_secs(1),
            },
            lan_discovery: false,
            ..ClientOptions::default()
        },
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_s_info_is_what_its_hello_says() {
    let stub = Stub::start().await;
    let client = client().await;
    client.add_address(&stub.id(), stub.addrs()).unwrap();
    let info = client.lan_server_info(&stub.id()).await.unwrap();
    assert_eq!(info.endpoint_id, stub.id());
    assert_eq!(info.name, "Stub");
    assert_eq!(
        Some(info.fingerprint.clone()),
        spoolframe_client_sdk::fingerprint::of_hex(&stub.id())
    );
    assert_eq!(info.server_version, "0");
    assert_eq!(info.protocol_version, PROTOCOL_VERSION);
    assert!(info.invite_only);
    client.close().await;
    stub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_is_not_invite_only_says_so() {
    let stub = Stub::start().await;
    stub.open_sign_up();
    let client = client().await;
    client.add_address(&stub.id(), stub.addrs()).unwrap();
    let info = client.lan_server_info(&stub.id()).await.unwrap();
    assert!(!info.invite_only);
    client.close().await;
    stub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn asking_saves_nothing() {
    let stub = Stub::start().await;
    let client = client().await;
    client.add_address(&stub.id(), stub.addrs()).unwrap();
    client.lan_server_info(&stub.id()).await.unwrap();
    assert!(client.servers().await.unwrap().iter().all(|e| !e.pinned));
    assert!(matches!(
        client.profiles(&stub.id()).await,
        Err(SdkError::NotPinned(_))
    ));
    client.close().await;
    stub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_that_is_not_one_is_refused() {
    let client = client().await;
    assert!(matches!(
        client.lan_server_info("not a key").await,
        Err(SdkError::UnknownServer(_))
    ));
    client.close().await;
}

/// The client does not listen on the LAN: nothing is on the network.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_off_the_lan_hears_no_server() {
    let stub = Stub::start().await;
    let client = client().await;
    client.add_address(&stub.id(), stub.addrs()).unwrap();
    client.connect(&stub.id()).await.unwrap();
    client.pin(&stub.id(), true).await.unwrap();
    assert!(client.lan_servers().await.unwrap().is_empty());
    client.close().await;
    stub.stop().await;
}
