//! What a server says of a profile's user is kept with the profile, but
//! a device that cannot write it still gets the server's answer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{Client, ClientOptions, Device, MemoryStorage, Timeouts};
use spoolframe_client_sdk::{Storage, StorageError};

use crate::stub::Stub;

/// Storage whose saved sessions (kept as secrets) cannot be written
/// once armed.
#[derive(Default)]
struct ReadOnlySessions {
    inner: MemoryStorage,
    armed: AtomicBool,
}

#[async_trait]
impl Storage for ReadOnlySessions {
    async fn secret(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.secret(key).await
    }
    async fn set_secret(&self, key: &str, value: Option<Vec<u8>>) -> Result<(), StorageError> {
        if key.starts_with("sessions/") && self.armed.load(Ordering::SeqCst) {
            return Err(StorageError("the keychain is locked".into()));
        }
        self.inner.set_secret(key, value).await
    }
    async fn data(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.data(key).await
    }
    async fn set_data(&self, key: &str, value: Option<Vec<u8>>) -> Result<(), StorageError> {
        self.inner.set_data(key, value).await
    }
}

/// `whoami` and `profiles` give the server's answer though noting it
/// fails: the note is a cache, not part of the answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_note_that_cannot_be_written_does_not_fail_the_answer() {
    let stub = Stub::start().await;
    let storage = Arc::new(ReadOnlySessions::default());
    let client = Client::open_with_options(
        storage.clone(),
        Device {
            name: "phone".into(),
            client_name: "sdk-noting-test".into(),
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
    // What is kept says another name: the server's answer is news.
    let key = format!("sessions/{}", stub.id());
    let kept = storage.inner.secret(&key).await.unwrap().unwrap();
    let mut kept: serde_json::Value = serde_json::from_slice(&kept).unwrap();
    for s in kept.as_array_mut().unwrap() {
        s["display_name"] = "Someone else".into();
    }
    storage
        .inner
        .set_secret(&key, Some(serde_json::to_vec(&kept).unwrap()))
        .await
        .unwrap();
    storage.armed.store(true, Ordering::SeqCst);

    let profiles = client.profiles(&stub.id()).await.unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].user().display_name, "alice");
    assert_eq!(session.whoami().await.unwrap().username, "alice");
    client.close().await;
    stub.stop().await;
}
