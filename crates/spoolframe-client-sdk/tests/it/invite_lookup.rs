//! Looking an invite up (spoolframe:invite-lookup) at a server that does
//! not list the feature, as one from before it: the SDK says so and sends
//! nothing but `Hello`, so the app shows the join form instead.

use std::sync::Arc;

use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{Client, ClientOptions, Device, Features, MemoryStorage, SdkError};

use crate::stub::Stub;

#[tokio::test(flavor = "multi_thread")]
async fn a_server_without_the_feature_is_unsupported_and_sent_no_lookup() {
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
            features: Features {
                invite_lookup: true,
                ..Features::default()
            },
            ..ClientOptions::default()
        },
    )
    .await
    .unwrap();
    let refused = client
        .look_up_invite_code(&stub.id(), stub.addrs(), "QW7K-2MFP-XR4T")
        .await;
    assert!(
        matches!(refused, Err(SdkError::Unsupported { ref feature }) if feature == "spoolframe:invite-lookup"),
        "{refused:?}"
    );
    assert_eq!(stub.count("hello"), 1);
    assert_eq!(stub.count("lookup_invite"), 0);
    // The app listed it: its Hello says so.
    assert_eq!(
        stub.hello_features(),
        [vec!["spoolframe:invite-lookup".to_owned()]]
    );
    client.close().await;
    stub.stop().await;
}
