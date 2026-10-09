//! A server's `web_url` is a page an app opens, or shows as a QR code a
//! phone's camera opens (PX-11, PX-09 finding 18). The SDK passes on only
//! an http or https URL: a malicious server's `file:` or app-scheme URL is
//! dropped, as if the server had no web UI.

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{Client, ClientOptions, Device, MemoryStorage, Timeouts};

use crate::stub::Stub;

fn device() -> Device {
    Device {
        name: "phone".into(),
        client_name: "sdk-web-url-test".into(),
        client_version: "0".into(),
        platform: Platform::Ios,
        capabilities: ClientCapabilities::default(),
    }
}

/// The `web_url` the SDK gives for a stub that sends `sent`: the signed-in
/// session's, and a device sign-in's.
async fn given(sent: &str) -> (Option<String>, Option<String>) {
    let stub = Stub::start().await;
    stub.web_url(sent);
    let client = Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        device(),
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
        .sign_in(&stub.id(), "alice", "pw", false)
        .await
        .unwrap();
    let sign_in = client
        .begin_device_sign_in(&stub.id(), false)
        .await
        .unwrap();
    let given = (session.web_url(), sign_in.web_url);
    client.close().await;
    stub.stop().await;
    given
}

#[tokio::test(flavor = "multi_thread")]
async fn a_web_url_that_is_not_http_or_https_is_dropped() {
    for sent in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "spoolframe://approve?code=BCDFGHJK",
        "data:text/html,<script>alert(1)</script>",
        "https://",
        "https://user@/profile",
        "https://media.example.org/\nfile:///etc/passwd",
        "not a url",
    ] {
        assert_eq!(given(sent).await, (None, None), "{sent}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_http_or_https_web_url_is_given_as_sent() {
    for sent in [
        "https://media.example.org",
        "http://10.10.0.6:7431",
        "http://[fd00::6]:7431/profile",
        "HTTPS://Media.Example.org/auth/device?code=BCDFGHJK",
    ] {
        let kept = Some(sent.to_owned());
        assert_eq!(given(sent).await, (kept.clone(), kept), "{sent}");
    }
}
