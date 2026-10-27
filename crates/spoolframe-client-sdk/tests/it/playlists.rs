//! The calls that were behind regular features (PX-16) before the
//! protocol's reset to revision 1 are base protocol: the SDK sends each to
//! any server, from an app that names nothing, and asks no list first.

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{
    Client, ClientOptions, Device, Kinds, MemoryStorage, Session, Timeouts, Visibility,
};
use spoolframe_types::{MediaId, MediaKind, Namespace};

use crate::stub::Stub;

/// Alice, signed in on `stub` from an app with the default options.
async fn alice(stub: &Stub) -> (Client, Session) {
    let client = Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        Device {
            name: "phone".into(),
            client_name: "sdk-playlists-test".into(),
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

/// Each call reaches the server, which answers it as it can (the stub
/// has none of them, and refuses each): the SDK no longer refuses one
/// itself for a feature the server did not list.
#[tokio::test]
async fn every_formerly_gated_call_is_sent() {
    let stub = Stub::start().await;
    let (_client, session) = alice(&stub).await;
    let film: MediaId = "plex:movie:1".parse().unwrap();
    let album = MediaId::new(Namespace::Plex, MediaKind::Album, "31").unwrap();
    let _ = session
        .create_playlist("Heists", Kinds::default(), Visibility::Personal)
        .await;
    let _ = session.playlists().await;
    let _ = session.remove_playlist_art("p").await;
    let _ = session.upcoming_in_playlist("p", 10).await;
    let _ = session
        .set_playlist_settings("p", spoolframe_client_sdk::PlaySettings::default())
        .await;
    let _ = session.like(std::slice::from_ref(&album)).await;
    let _ = session.share_candidates("p", "", 0, 10).await;
    let _ = session.sync_subtitle(&film, 0).await;
    let _ = session.use_original_subtitle_timing(&film, 0).await;
    let _ = session.set_subtitle_offset(&film, 0, 1_500).await;
    let _ = session.search_subtitles(&film, None).await;
    let _ = session.download_subtitle(&film, "opensubtitles:101").await;
    let _ = session.remove_subtitle_download(&film, "d1").await;
    let _ = session.recently_played(None, 0, 10).await;
    let sent: Vec<(&str, usize)> = [
        "create_playlist",
        "list_playlists",
        "remove_playlist_art",
        "upcoming_in_playlist",
        "set_playlist_settings",
        "like",
        "list_share_candidates",
        "sync_subtitle",
        "use_original_subtitle_timing",
        "set_subtitle_offset",
        "search_subtitles",
        "download_subtitle",
        "remove_subtitle_download",
        "recently_played",
    ]
    .into_iter()
    .map(|kind| (kind, stub.count(kind)))
    .collect();
    assert!(sent.iter().all(|(_, n)| *n == 1), "{sent:?}");
}
