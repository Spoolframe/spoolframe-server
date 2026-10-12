//! A one-time password an admin reset to signs in to a session that may
//! only change it: the SDK says so on the session, and changes it.

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{Client, ClientOptions, Device, MemoryStorage, Timeouts};

use crate::stub::Stub;

/// Sign alice in with `password` and change it: whether the session asked
/// for a change before, what the change answered, and whether the session
/// and a clone of it made before still ask after.
async fn signed_in_with(
    password: &str,
) -> (
    bool,
    Result<(), spoolframe_client_sdk::SdkError>,
    bool,
    bool,
) {
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
    let clone = session.clone();
    let changed = session
        .change_password(password, "a new password of hers")
        .await;
    let after = (
        session.password_change_required(),
        clone.password_change_required(),
    );
    client.close().await;
    stub.stop().await;
    (required, changed, after.0, after.1)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_one_time_password_s_session_asks_for_a_new_one() {
    let (required, changed, _, _) = signed_in_with("one-time").await;
    assert!(required);
    changed.unwrap();
}

/// C5: once changed, the session (and every clone of it) no longer asks.
#[tokio::test(flavor = "multi_thread")]
async fn a_changed_one_time_password_is_no_longer_asked_for() {
    let (_, changed, after, clone_after) = signed_in_with("one-time").await;
    changed.unwrap();
    assert!(!after, "the session still asks for a new password");
    assert!(!clone_after, "a clone still asks for a new password");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ordinary_password_s_session_does_not() {
    let (required, _, _, _) = signed_in_with("correct horse").await;
    assert!(!required);
}
