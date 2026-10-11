//! `Client::reachability`: what an app shows under "can't be reached".

use std::sync::Arc;

use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{
    Client, ClientOptions, Device, DialStage, MemoryStorage, PathKind, SdkError,
};

use crate::stub::Stub;

async fn client() -> Client {
    let device = Device {
        name: "phone".into(),
        client_name: "sdk-reachability-test".into(),
        client_version: "0".into(),
        platform: Platform::Ios,
        capabilities: ClientCapabilities::default(),
    };
    let options = ClientOptions {
        lan_discovery: false,
        ..ClientOptions::default()
    };
    Client::open_with_options(Arc::new(MemoryStorage::default()), device, options)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_saved_connected_server_shows_its_dial_and_live_path() {
    let stub = Stub::start().await;
    let client = client().await;
    client.add_address(&stub.id(), stub.addrs()).unwrap();
    client.connect(&stub.id()).await.unwrap();
    client.pin(&stub.id(), true).await.unwrap();

    let reach = client.reachability(&stub.id()).await.unwrap();
    assert!(reach.saved);
    assert!(!reach.addrs.is_empty(), "{reach:?}");
    assert!(reach.relays_effective, "relays are on by default");
    let dial = reach.last_dial.expect("dialled");
    assert!(dial.connected, "{dial:?}");
    assert_eq!((dial.stage, dial.error), (None, None));
    assert!(!dial.dialled.is_empty());
    let selected = reach.live.first().expect("a live path");
    assert!(selected.selected, "{:?}", reach.live);
    assert_ne!(selected.kind, PathKind::Relay);
    stub.stop().await;
}

/// It answered, and refused: the stage says so, and the error is the one
/// the app was given.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_hello_is_a_refused_dial() {
    let stub = Stub::start().await;
    stub.refuse_hellos();
    let client = client().await;
    client.add_address(&stub.id(), stub.addrs()).unwrap();
    let error = client.connect(&stub.id()).await.unwrap_err();

    let reach = client.reachability(&stub.id()).await.unwrap();
    assert!(!reach.saved);
    assert_eq!(reach.hinted_addrs.len(), stub.addrs().len());
    let dial = reach.last_dial.expect("dialled");
    assert!(!dial.connected);
    assert_eq!(dial.stage, Some(DialStage::Refused));
    assert_eq!(dial.error, Some(error.to_string()));
    assert!(reach.live.is_empty());
    stub.stop().await;
}

/// Nothing answers at the one address known: no answer in time, and iroh
/// knew of that address.
#[tokio::test(flavor = "multi_thread")]
async fn an_address_nothing_answers_at_is_a_timeout() {
    let stub = Stub::start().await;
    let (id, addrs) = (stub.id(), stub.addrs());
    stub.stop().await;
    let client = client().await;
    client.add_address(&id, addrs.clone()).unwrap();
    assert!(client.connect(&id).await.is_err());

    let dial = client.reachability(&id).await.unwrap().last_dial.unwrap();
    assert_eq!(dial.stage, Some(DialStage::Timeout), "{dial:?}");
    for addr in &addrs {
        assert!(dial.dialled.contains(&addr.to_string()), "{dial:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_nothing_is_known_of_is_unknown() {
    let client = client().await;
    let id = iroh::SecretKey::from_bytes(&[9; 32]).public().to_string();
    assert!(matches!(
        client.reachability(&id).await,
        Err(SdkError::UnknownServer(_))
    ));
}
