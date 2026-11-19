//! A device sign-in approved while its connection is cut, again and
//! again: each round is asked again as the same operation, and the
//! server's replayed answer brings the one session it made.

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{Client, ClientOptions, Device, MemoryStorage, Timeouts};

use crate::stub::Stub;

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_lost_to_cut_connections_is_replayed_to_the_same_operation() {
    let stub = Stub::start().await;
    let tv = Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        Device {
            name: "tv".into(),
            client_name: "sdk-replay-test".into(),
            client_version: "0".into(),
            platform: Platform::Tvos,
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
    tv.add_address(&stub.id(), stub.addrs()).unwrap();
    tv.connect(&stub.id()).await.unwrap();
    tv.pin(&stub.id(), true).await.unwrap();
    let sign_in = tv.begin_device_sign_in(&stub.id(), false).await.unwrap();
    // More cuts than one reconnect covers.
    stub.approve_and_lose_answers(3);

    let session = tv.await_device_sign_in(&sign_in).await.unwrap();
    assert_eq!(session.user().username, "alice");
    assert_eq!(stub.device_sessions(), 1);
    let asked = stub.invocations("await_device_sign_in");
    assert!(asked.len() >= 4, "{}", asked.len());
    let operations: std::collections::HashSet<_> = asked
        .iter()
        .map(|w| w.operation.as_ref().map(|o| o.id.clone()))
        .collect();
    assert_eq!(operations.len(), 1, "{operations:?}");
    tv.close().await;
    stub.stop().await;
}
