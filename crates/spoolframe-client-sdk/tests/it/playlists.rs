//! Playlists (PX-40) from a server without them: the SDK refuses the call
//! itself, and sends nothing.

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{
    Client, ClientOptions, Device, Kinds, MemoryStorage, SdkError, Timeouts, Visibility,
};

use crate::stub::Stub;

#[tokio::test]
async fn a_server_without_playlists_is_not_asked_for_them() {
    let stub = Stub::start().await;
    let client = Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        Device {
            name: "phone".into(),
            client_name: "sdk-playlists-test".into(),
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
    assert!(!session.features().playlists);
    let made = session
        .create_playlist("Heists", Kinds::default(), Visibility::Personal)
        .await;
    assert_eq!(
        made.unwrap_err(),
        SdkError::Unsupported {
            feature: "spoolframe:playlists".into()
        }
    );
    assert!(matches!(
        session.playlists().await,
        Err(SdkError::Unsupported { .. })
    ));
    assert!(stub.requests_of("create_playlist").is_empty());
    assert!(stub.requests_of("list_playlists").is_empty());
}
