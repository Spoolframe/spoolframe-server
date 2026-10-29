//! A feature's calls are reached only where the feature is on: nothing of
//! them is sent to a server that did not name it.

use std::sync::Arc;

use spoolframe_client_sdk::gate::InvitePasskeys;
use spoolframe_client_sdk::invite::Invite;
use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{
    Client, ClientOptions, Device, Feature, Features, MemoryStorage, SdkError,
};

use crate::stub::Stub;

async fn client(features: Features) -> Client {
    Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        Device {
            name: "phone".into(),
            client_name: "sdk-features-test".into(),
            client_version: "0".into(),
            platform: Platform::Ios,
            capabilities: ClientCapabilities::default(),
        },
        ClientOptions {
            lan_discovery: false,
            features,
            ..ClientOptions::default()
        },
    )
    .await
    .unwrap()
}

/// An invite to the stub, with a code.
fn link(stub: &Stub) -> String {
    Invite {
        server: stub.id(),
        name: Some("Stub".into()),
        addrs: stub.addrs(),
        relays: Vec::new(),
        lookup: None,
        code: Some("QW7K2MFPXR4T".into()),
        expires_at: None,
        role: None,
        kind: Default::default(),
    }
    .link()
}

const OFF: SdkError = SdkError::FeatureOff {
    feature: Feature::InvitePasskeys,
};

/// A server that does not have the feature: refused after its `Hello`,
/// and none of the feature's requests sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_without_the_feature_is_sent_none_of_its_requests() {
    let stub = Stub::start().await;
    let client = client(Features::all()).await;

    let refused = client.invited::<InvitePasskeys>(&link(&stub)).await;

    assert_eq!(refused.unwrap_err(), OFF);
    assert_eq!(stub.count("hello"), 1);
    assert_eq!(stub.count("begin_invite_passkey"), 0);
    client.close().await;
    stub.stop().await;
}

/// The server has it but the app did not name it: off, as both must.
#[tokio::test(flavor = "multi_thread")]
async fn a_feature_the_app_did_not_name_is_off() {
    let stub = Stub::start().await;
    stub.have_features(Features::all());
    let client = client(Features::empty()).await;

    let refused = client.invited::<InvitePasskeys>(&link(&stub)).await;

    assert_eq!(refused.unwrap_err(), OFF);
    client.close().await;
    stub.stop().await;
}

/// The feature on when its calls were reached, and off on the connection
/// they go on: the request is stopped in the SDK, never sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_feature_gone_off_since_stops_its_request_unsent() {
    let stub = Stub::start().await;
    stub.have_features(Features::all());
    let client = client(Features::all()).await;
    let invited = client
        .invited::<InvitePasskeys>(&link(&stub))
        .await
        .unwrap();
    // The server restarts as one without it.
    stub.have_features(Features::empty());
    stub.close_open_connections();

    let refused = invited.begin_invite_passkey("pia", None).await;

    assert_eq!(refused.unwrap_err(), OFF);
    assert_eq!(stub.count("hello"), 2, "it was asked on a new connection");
    assert_eq!(stub.count("begin_invite_passkey"), 0);
    client.close().await;
    stub.stop().await;
}
