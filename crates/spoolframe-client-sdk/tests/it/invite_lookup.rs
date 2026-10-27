//! Looking an invite up is base protocol: an app with the default options
//! asks any server, with nothing named in its `Hello`.

use std::sync::Arc;

use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{Client, ClientOptions, Device, MemoryStorage};

use crate::stub::Stub;

#[tokio::test(flavor = "multi_thread")]
async fn a_code_is_looked_up_at_any_server() {
    let stub = Stub::start().await;
    let client = Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        Device {
            name: "phone".into(),
            client_name: "sdk-invite-lookup-test".into(),
            client_version: "0".into(),
            platform: Platform::Ios,
            capabilities: ClientCapabilities::default(),
        },
        ClientOptions {
            lan_discovery: false,
            ..ClientOptions::default()
        },
    )
    .await
    .unwrap();
    // The stub has no invites, and refuses the lookup: it was sent.
    let _ = client
        .look_up_invite_code(&stub.id(), stub.addrs(), "QW7K-2MFP-XR4T")
        .await;
    assert_eq!((stub.count("hello"), stub.count("lookup_invite")), (1, 1));
    client.close().await;
    stub.stop().await;
}
