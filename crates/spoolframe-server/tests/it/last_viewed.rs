//! When each item was last viewed, over a real
//! Iroh connection: what the Apple TV's Top Shelf needs to show only what
//! was watched in the last two days, from any device of the user's.

use crate::support;

use std::time::Duration;

use spoolframe_protocol::{Hello, PROTOCOL_VERSION, Reply, Request, Response, SessionId};
use spoolframe_types::{Item, MediaId};
use support::{TestClient, TestServer, movie};

use crate::watch::{home, progress, state_of};

/// A client that plays MKV with H.264.
async fn device(server: &TestServer) -> TestClient {
    let client = server.client().await;
    let mut capabilities = support::mkv_capabilities();
    capabilities
        .video
        .insert(spoolframe_types::VideoCodec::H264);
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "test".into(),
        client_version: "0".into(),
        platform: spoolframe_types::Platform::Linux,
        device_name: None,
        capabilities: Some(capabilities),
        experimental: Vec::new(),
        features: spoolframe_protocol::Features::empty(),
    };
    assert!(matches!(
        client.call(None, Request::Hello(hello)).await,
        Response::Ok(Reply::ServerInfo(_))
    ));
    client
}

/// The first Continue watching item's `last_viewed_at`.
async fn on_home(tv: &TestClient, who: &SessionId) -> Option<std::time::SystemTime> {
    match home(tv, who).await.continue_watching.first() {
        Some(Item::Movie(m)) => m.playback.last_viewed_at,
        Some(Item::Episode(e)) => e.playback.last_viewed_at,
        other => panic!("nothing to continue: {other:?}"),
    }
}

/// A TV sees when the film was last viewed, on Home and on its details;
/// it moves when the same user's phone reports progress an hour later,
/// and not when another user's does. Neither names a feature.
#[tokio::test]
async fn the_last_view_is_any_device_s_and_the_user_s_own() {
    let server = TestServer::start("last-viewed").await;
    server.add_user("alice", "correct horse", None).await;
    server.add_user("bob", "battery staple", None).await;
    let tv = device(&server).await;
    let phone = device(&server).await;
    let on_tv = tv.login("alice", "correct horse").await;
    let on_phone = phone.login("alice", "correct horse").await;
    let bob = phone.login("bob", "battery staple").await;
    let film: MediaId = movie().id().clone();

    progress(&server, &tv, &on_tv, &film, 600).await;
    let first = on_home(&tv, &on_tv).await.expect("a time on Home");
    assert_eq!(
        state_of(&tv, &on_tv, &film).await.last_viewed_at,
        Some(first)
    );

    server.clock.advance(Duration::from_secs(3600));
    progress(&server, &phone, &on_phone, &film, 900).await;
    let later = on_home(&tv, &on_tv).await.expect("a time on Home");
    assert!(
        later >= first + Duration::from_secs(3600),
        "{first:?} then {later:?}"
    );

    server.clock.advance(Duration::from_secs(3600));
    progress(&server, &phone, &bob, &film, 300).await;
    assert_eq!(on_home(&tv, &on_tv).await, Some(later));

    // The phone is sent the same.
    assert_eq!(on_home(&phone, &on_phone).await, Some(later));
    assert_eq!(
        state_of(&phone, &on_phone, &film).await.last_viewed_at,
        Some(later)
    );
    tv.close().await;
    phone.close().await;
    server.stop().await;
}
