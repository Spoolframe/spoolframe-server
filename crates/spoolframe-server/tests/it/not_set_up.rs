//! A server that is not set up yet (P3-10): no catalog. Clients sign in,
//! and every catalog and playback request is `NotSetUp`, which the SDK
//! hands the app as its own error.

use crate::support;

use std::sync::Arc;

use spoolframe_client_sdk::{Client, Device, MemoryStorage, SdkError};
use support::{ServerOptions, TestServer, fresh_dir, library, mkv_capabilities, movie};

#[tokio::test]
async fn clients_sign_in_and_are_told_the_server_is_not_set_up() {
    let server = TestServer::start_with(
        fresh_dir("clients").await,
        ServerOptions {
            not_set_up: true,
            ..ServerOptions::default()
        },
    )
    .await;
    server.add_user("alice", "correct horse", None).await;
    let client = Client::open(
        Arc::new(MemoryStorage::default()),
        Device {
            name: "living room".into(),
            client_name: "sdk-test".into(),
            client_version: "0".into(),
            platform: spoolframe_client_sdk::types::Platform::Cli,
            capabilities: mkv_capabilities(),
        },
    )
    .await
    .unwrap();
    let id = server.server.id().to_string();
    client
        .add_address(&id, server.server.addr().ip_addrs().copied().collect())
        .unwrap();
    client.connect(&id).await.unwrap();
    client.pin(&id, true).await.unwrap();
    let alice = client.sign_in(&id, "alice", "correct horse").await.unwrap();
    assert_eq!(alice.whoami().await.unwrap().username, "alice");

    assert_eq!(alice.libraries().await.unwrap_err(), SdkError::NotSetUp);
    assert_eq!(
        alice.items(&library().id, 0, 50).await.unwrap_err(),
        SdkError::NotSetUp
    );
    assert_eq!(
        alice.search(support::search("ali")).await.unwrap_err(),
        SdkError::NotSetUp
    );
    assert_eq!(alice.home().await.unwrap_err(), SdkError::NotSetUp);
    assert_eq!(
        alice.start_playback(movie().id(), None).await.unwrap_err(),
        SdkError::NotSetUp
    );
    assert_eq!(
        SdkError::NotSetUp.to_string(),
        "this server is not set up yet"
    );
    client.close().await;
    server.stop().await;
}
