//! A one-time password an admin reset to signs in to a session that may
//! only change it: the SDK says so on the session, and changes it.

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{Client, ClientOptions, Device, MemoryStorage, Timeouts};

use crate::stub::Stub;

async fn signed_in_with(password: &str) -> (bool, Result<(), spoolframe_client_sdk::SdkError>) {
    let stub = Stub::start().await;
    let client = Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        Device {
            name: "phone".into(),
            client_name: "sdk-one-time-password-test".into(),
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
        .sign_in(&stub.id(), "alice", password, false)
        .await
        .unwrap();
    let required = session.password_change_required();
    let changed = session
        .change_password(password, "a new password of hers")
        .await;
    client.close().await;
    stub.stop().await;
    (required, changed)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_one_time_password_s_session_asks_for_a_new_one() {
    let (required, changed) = signed_in_with("one-time").await;
    assert!(required);
    changed.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ordinary_password_s_session_does_not() {
    let (required, _) = signed_in_with("correct horse").await;
    assert!(!required);
}
