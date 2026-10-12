//! Closing a server's link (C6) stops its supervisor between steps, never
//! in the middle of one: a write to storage it started is finished.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use spoolframe_client_sdk::storage::{StorageError, keys};
use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{Client, ClientOptions, Device, MemoryStorage, Storage};
use tokio::sync::Notify;

use crate::stub::Stub;

/// No test takes this long unless something hangs.
const HANG: Duration = Duration::from_secs(60);

/// Memory storage whose next write of the saved servers, once `armed`,
/// waits for `release` before it is made: a slow disk, caught mid-write.
#[derive(Default)]
struct Gated {
    inner: MemoryStorage,
    armed: AtomicBool,
    entered: Notify,
    release: Notify,
    finished: AtomicBool,
}

#[async_trait]
impl Storage for Gated {
    async fn secret(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.secret(key).await
    }
    async fn set_secret(&self, key: &str, value: Option<Vec<u8>>) -> Result<(), StorageError> {
        self.inner.set_secret(key, value).await
    }
    async fn data(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.data(key).await
    }
    async fn set_data(&self, key: &str, value: Option<Vec<u8>>) -> Result<(), StorageError> {
        if key == keys::SERVERS && self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
            self.inner.set_data(key, value).await?;
            self.finished.store(true, Ordering::SeqCst);
            return Ok(());
        }
        self.inner.set_data(key, value).await
    }
}

/// The addresses saved for server `id`.
async fn saved_addrs(client: &Client, id: &str) -> Vec<SocketAddr> {
    client
        .servers()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.endpoint_id == id)
        .map(|s| s.addrs)
        .unwrap_or_default()
}

/// The server pushes its addresses, and the supervisor is saving them
/// when the user forgets the server: the save is finished, not cut off
/// halfway, before the server is forgotten.
#[tokio::test(flavor = "multi_thread")]
async fn forgetting_a_server_lets_a_save_under_way_finish() {
    tokio::time::timeout(HANG, async {
        let stub = Stub::start().await;
        let first: SocketAddr = "192.0.2.1:4433".parse().unwrap();
        let second: SocketAddr = "192.0.2.2:4433".parse().unwrap();
        stub.tell(&[first]);
        let storage = Arc::new(Gated::default());
        let client = Client::open_with_options(
            storage.clone(),
            Device {
                name: "phone".into(),
                client_name: "sdk-closing-test".into(),
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
        client.add_address(&stub.id(), stub.addrs()).unwrap();
        client.connect(&stub.id()).await.unwrap();
        client.pin(&stub.id(), true).await.unwrap();
        client
            .sign_in(&stub.id(), "alice", "correct horse", false)
            .await
            .unwrap();
        while !saved_addrs(&client, &stub.id()).await.contains(&first) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        storage.armed.store(true, Ordering::SeqCst);
        stub.tell(&[second]);
        stub.push().await;
        storage.entered.notified().await;

        // The supervisor is inside its save: forget closes the link now.
        let forget = tokio::spawn({
            let (client, id) = (client.clone(), stub.id());
            async move { client.forget(&id).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        storage.release.notify_one();
        forget.await.unwrap().unwrap();
        assert!(
            storage.finished.load(Ordering::SeqCst),
            "the save under way was cut off"
        );
        client.close().await;
        stub.stop().await;
    })
    .await
    .expect("the test hung");
}
