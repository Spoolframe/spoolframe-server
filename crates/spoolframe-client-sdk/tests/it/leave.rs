//! Leaving a server (PX-47): the SDK asks the server, and forgets it on
//! this device only when the server says it is done.

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{
    Client, ClientOptions, Device, MemoryStorage, SdkError, Session, Timeouts,
};

use crate::stub::Stub;

/// A client with alice signed in to `stub`, saved and pinned.
async fn signed_in(stub: &Stub) -> (Client, Session) {
    let client = Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        Device {
            name: "phone".into(),
            client_name: "sdk-leave-test".into(),
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
    (client, session)
}

/// Sign alice in to a saved stub, leave it, and say what the leave
/// answered and whether the stub is still saved here.
async fn leave(refused: bool) -> (Result<(), SdkError>, bool) {
    let stub = Stub::start().await;
    if refused {
        stub.refuse_leaving();
    }
    let (client, session) = signed_in(&stub).await;
    let left = session.leave_server().await;
    let saved = client
        .servers()
        .await
        .unwrap()
        .iter()
        .any(|s| s.endpoint_id == stub.id() && s.pinned);
    client.close().await;
    stub.stop().await;
    (left, saved)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_left_is_forgotten_here() {
    let (left, saved) = leave(false).await;
    left.unwrap();
    assert!(!saved);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_last_admin_is_refused_and_the_server_stays_saved() {
    let (left, saved) = leave(true).await;
    assert_eq!(left, Err(SdkError::LastAdmin));
    assert!(saved);
}

/// What `WhoAmI` said about leaving when the session was made, and what
/// the server says now.
async fn can_leave(refused_before: bool, refused_after: bool) -> (bool, bool) {
    let stub = Stub::start().await;
    if refused_before {
        stub.refuse_leaving();
    }
    let (client, session) = signed_in(&stub).await;
    if refused_after {
        stub.refuse_leaving();
    }
    let answer = (session.can_leave(), session.can_leave_now().await.unwrap());
    client.close().await;
    stub.stop().await;
    answer
}

#[tokio::test(flavor = "multi_thread")]
async fn the_last_admin_is_told_ahead_that_they_cannot_leave() {
    assert_eq!(can_leave(true, true).await, (false, false));
}

#[tokio::test(flavor = "multi_thread")]
async fn anyone_else_is_told_they_can_leave() {
    assert_eq!(can_leave(false, false).await, (true, true));
}

/// A session says what `WhoAmI` said when it was made; asking again reads
/// a change since.
#[tokio::test(flavor = "multi_thread")]
async fn can_leave_is_as_of_sign_in_until_asked_again() {
    assert_eq!(can_leave(false, true).await, (true, false));
}
