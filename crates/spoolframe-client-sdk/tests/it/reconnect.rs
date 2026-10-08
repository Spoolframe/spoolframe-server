//! The supervisor gives up on what dialling again cannot fix: a server of
//! another protocol revision, and a server that is not the one pinned. It
//! dials once more after the connection is lost, sees why, and stops with
//! the reason on the state watch, where it used to dial every 10 s for as
//! long as the app ran.

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{
    Client, ClientOptions, ConnectionState, Device, MemoryStorage, SdkError, StateWatch,
};

use crate::stub::Stub;

/// No test takes this long unless the SDK never stops dialling.
const HANG: Duration = Duration::from_secs(20);

/// Long enough for a supervisor that did not stop to dial again a few
/// times: its first waits are 250 ms, 500 ms and 1 s.
const SETTLE: Duration = Duration::from_secs(2);

async fn client() -> Client {
    let device = Device {
        name: "phone".into(),
        client_name: "sdk-reconnect-test".into(),
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

/// A client with `stub` pinned and connected, and the stub's state watch.
async fn connected(stub: &Stub) -> (Client, StateWatch) {
    let client = client().await;
    client.add_address(&stub.id(), stub.addrs()).unwrap();
    client.connect(&stub.id()).await.unwrap();
    client.pin(&stub.id(), true).await.unwrap();
    let state = client.state(&stub.id()).unwrap();
    assert_eq!(state.current(), ConnectionState::Connected);
    (client, state)
}

/// Why the supervisor stopped, once it has; fails if it dials on.
async fn stopped(state: &mut StateWatch) -> SdkError {
    tokio::time::timeout(HANG, async {
        loop {
            if state.current() == ConnectionState::Disconnected
                && let Some(reason) = state.stopped()
            {
                return reason;
            }
            state.changed().await.expect("the client is open");
        }
    })
    .await
    .expect("the SDK went on dialling")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_refuses_the_protocol_revision_is_not_dialled_again() {
    let stub = Stub::start().await;
    let (client, mut state) = connected(&stub).await;
    stub.refuse_hellos();
    stub.close_open_connections();

    let reason = stopped(&mut state).await;
    assert_eq!(
        reason,
        SdkError::UpdateRequired {
            server_protocol: None
        }
    );
    tokio::time::sleep(SETTLE).await;
    // The first connect, and the one redial that was refused.
    assert_eq!(stub.count("hello"), 2);
    assert_eq!(state.current(), ConnectionState::Disconnected);
    client.close().await;
    stub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_of_another_protocol_revision_names_it() {
    let stub = Stub::start().await;
    let (client, mut state) = connected(&stub).await;
    let newer = spoolframe_protocol::PROTOCOL_VERSION + 1;
    stub.speak_protocol(newer);
    stub.close_open_connections();

    assert_eq!(
        stopped(&mut state).await,
        SdkError::UpdateRequired {
            server_protocol: Some(newer)
        }
    );
    tokio::time::sleep(SETTLE).await;
    assert_eq!(stub.count("hello"), 2);
    client.close().await;
    stub.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_answering_with_another_server_id_is_not_dialled_again() {
    let stub = Stub::start().await;
    let (client, mut state) = connected(&stub).await;
    let other = spoolframe_protocol::ServerId::from_bytes([9; 10]).to_string();
    stub.answer_as_server_id(&other);
    stub.close_open_connections();

    let reason = stopped(&mut state).await;
    assert!(
        matches!(&reason, SdkError::ServerIdChanged { now, .. } if *now == other),
        "{reason:?}"
    );
    tokio::time::sleep(SETTLE).await;
    assert_eq!(stub.count("hello"), 2);
    client.close().await;
    stub.stop().await;
}

/// The reason is the link's only while it stays stopped: a call that
/// dials and connects clears it.
#[tokio::test(flavor = "multi_thread")]
async fn a_connect_after_the_cause_is_fixed_clears_the_reason() {
    let stub = Stub::start().await;
    let (client, mut state) = connected(&stub).await;
    stub.speak_protocol(spoolframe_protocol::PROTOCOL_VERSION + 1);
    stub.close_open_connections();
    stopped(&mut state).await;

    stub.speak_protocol(spoolframe_protocol::PROTOCOL_VERSION);
    client.connect(&stub.id()).await.unwrap();
    assert_eq!(state.current(), ConnectionState::Connected);
    assert_eq!(state.stopped(), None);
    client.close().await;
    stub.stop().await;
}
