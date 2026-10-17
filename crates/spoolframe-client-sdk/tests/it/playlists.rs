//! Playlists (PX-40) and the other regular features (PX-16) the SDK
//! gates: from a server without them, or in an app that did not list them,
//! the SDK refuses the call itself, and sends nothing.

use std::sync::Arc;
use std::time::Duration;

use spoolframe_client_sdk::types::{ClientCapabilities, Platform};
use spoolframe_client_sdk::{
    Client, ClientOptions, Device, Features, Kinds, MemoryStorage, SdkError, Session, Timeouts,
    Visibility,
};
use spoolframe_protocol::features::{LIKED, PLAYLISTS};
use spoolframe_types::{MediaId, MediaKind, Namespace};

use crate::stub::Stub;

/// Every regular feature this SDK knows, as an app that handles them all.
const ALL: Features = Features {
    playlists: true,
    liked: true,
    last_viewed: true,
    playlist_sharing: true,
    play_history: true,
    playlist_art: true,
    subtitle_sync: true,
};

/// Alice, signed in on `stub` from an app that lists `features`.
async fn alice_listing(stub: &Stub, features: Features) -> (Client, Session) {
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
            features,
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

#[tokio::test]
async fn a_server_without_playlists_is_not_asked_for_them() {
    let stub = Stub::start().await;
    let (_client, session) = alice_listing(&stub, ALL).await;
    assert!(!session.features().playlists);
    let made = session
        .create_playlist("Heists", Kinds::default(), Visibility::Personal)
        .await;
    assert_eq!(
        made.unwrap_err(),
        SdkError::Unsupported {
            feature: "spoolframe:playlists".into()
        }
    );
    assert!(matches!(
        session.playlists().await,
        Err(SdkError::Unsupported { .. })
    ));
    assert!(stub.requests_of("create_playlist").is_empty());
    assert!(stub.requests_of("list_playlists").is_empty());
}

/// An app that lists no feature, the default, is sent none and has none,
/// though the server has them all: an SDK knowing a feature does not turn
/// it on for an app whose code does not handle it.
#[tokio::test]
async fn by_default_an_app_lists_no_feature_and_has_none() {
    let stub = Stub::start().await;
    stub.have_all_features();
    let (client, session) = alice_listing(&stub, Features::default()).await;
    assert_eq!(stub.hello_features(), vec![Vec::<String>::new()]);
    assert_eq!(session.features(), Features::default());
    // Found again rather than made: from `WhoAmI`, the same.
    let found = client.profiles(&stub.id()).await.unwrap();
    assert_eq!(found[0].features(), Features::default());
}

/// An app that lists playlists and Liked is sent exactly those, and has
/// only those of the server's.
#[tokio::test]
async fn an_app_has_only_the_features_it_lists() {
    let stub = Stub::start().await;
    stub.have_all_features();
    let listed = Features {
        playlists: true,
        liked: true,
        ..Features::default()
    };
    let (_client, session) = alice_listing(&stub, listed).await;
    assert_eq!(
        stub.hello_features(),
        vec![vec![PLAYLISTS.to_owned(), LIKED.to_owned()]]
    );
    assert_eq!(session.features(), listed);
}

/// An app that did not list playlist covers cannot upload or remove one,
/// even on a server that has them: the SDK refuses, and sends nothing.
#[tokio::test]
async fn an_app_not_listing_playlist_art_does_not_send_a_cover() {
    let stub = Stub::start().await;
    stub.have_all_features();
    let listed = Features {
        playlists: true,
        ..Features::default()
    };
    let (_client, session) = alice_listing(&stub, listed).await;
    let unsupported = SdkError::Unsupported {
        feature: spoolframe_protocol::features::PLAYLIST_ART.into(),
    };
    assert_eq!(
        session
            .set_playlist_art("p", b"\xff\xd8\xff".to_vec())
            .await
            .unwrap_err(),
        unsupported
    );
    assert_eq!(
        session.remove_playlist_art("p").await.unwrap_err(),
        unsupported
    );
    assert!(stub.requests_of("remove_playlist_art").is_empty());
}

/// PX-03: an app that did not list subtitle sync cannot sync, set back or
/// offset a track, even on a server that has it: the SDK refuses, and
/// sends nothing.
#[tokio::test]
async fn an_app_not_listing_subtitle_sync_does_not_retime_a_track() {
    let stub = Stub::start().await;
    stub.have_all_features();
    let (_client, session) = alice_listing(&stub, Features::default()).await;
    let unsupported = SdkError::Unsupported {
        feature: spoolframe_protocol::features::SUBTITLE_SYNC.into(),
    };
    let id: spoolframe_client_sdk::types::MediaId = "plex:movie:1".parse().unwrap();
    assert_eq!(
        session.sync_subtitle(&id, 0).await.unwrap_err(),
        unsupported
    );
    assert_eq!(
        session
            .use_original_subtitle_timing(&id, 0)
            .await
            .unwrap_err(),
        unsupported
    );
    assert_eq!(
        session
            .set_subtitle_offset(&id, 0, 1_500)
            .await
            .unwrap_err(),
        unsupported
    );
    assert!(stub.requests_of("sync_subtitle").is_empty());
    assert!(stub.requests_of("set_subtitle_offset").is_empty());
}

/// An app that did not list Liked cannot like, even on a server that has
/// it: the SDK refuses, and sends nothing.
#[tokio::test]
async fn an_app_not_listing_liked_does_not_send_a_like() {
    let stub = Stub::start().await;
    stub.have_all_features();
    let listed = Features {
        playlists: true,
        ..Features::default()
    };
    let (_client, session) = alice_listing(&stub, listed).await;
    let album = MediaId::new(Namespace::Plex, MediaKind::Album, "31").unwrap();
    assert_eq!(
        session
            .like(std::slice::from_ref(&album))
            .await
            .unwrap_err(),
        SdkError::Unsupported {
            feature: LIKED.into()
        }
    );
    assert!(stub.requests_of("like").is_empty());
}
