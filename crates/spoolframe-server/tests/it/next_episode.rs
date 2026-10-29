//! What plays after an episode, and where a show's Continue goes (feature
//! `spoolframe:next-episode`), over the PMS of `watch::pms_with_a_show`:
//! specials 201, season 1 (211, 212) and season 2 (221, 222).

use std::sync::Arc;

use spoolframe_client_ffi::SpoolSession;
use spoolframe_client_sdk::gate::NextEpisodes;
use spoolframe_client_sdk::{MemoryStorage, NextMode};
use spoolframe_plex::PlexCatalog;
use spoolframe_protocol::{Error, Feature, Features, Reply, Request, Response, SessionId};
use spoolframe_server::catalog::{Composite, LiveCatalog};
use spoolframe_server::principal::Principal;
use spoolframe_server::store::watch::WatchUpdate;
use spoolframe_types::{Item, MediaId, MediaKind, Namespace};
use wiremock::MockServer;

use crate::support::{self, ServerOptions, TestClient, TestServer};

fn episode(key: u32) -> MediaId {
    MediaId::new(Namespace::Plex, MediaKind::Episode, key.to_string()).unwrap()
}

fn show() -> MediaId {
    MediaId::new(Namespace::Plex, MediaKind::Show, "100").unwrap()
}

/// A server over the show's PMS, with alice.
async fn server(test: &str) -> (TestServer, MockServer) {
    let pms = crate::watch::pms_with_a_show().await;
    let plex = PlexCatalog::new(pms.uri().parse().unwrap(), None).unwrap();
    let live = LiveCatalog::new(Some(Arc::new(Composite::single("fulcrum-ng", plex))));
    let options = ServerOptions {
        catalog: Some(Arc::new(live)),
        ..ServerOptions::default()
    };
    let server = TestServer::start_with(support::fresh_dir(test).await, options).await;
    server.add_user("alice", "correct horse", None).await;
    (server, pms)
}

/// Alice on a device whose `Hello` named the feature.
async fn alice(server: &TestServer) -> (TestClient, SessionId) {
    let tv = server.client().await;
    let info = tv
        .hello_with_features(Features::only(Feature::NextEpisodes))
        .await;
    assert!(info.features.contains(Feature::NextEpisodes));
    let session = tv.login("alice", "correct horse").await;
    (tv, session)
}

/// What `request` found: its id, or `None`.
async fn found(tv: &TestClient, session: &SessionId, request: Request) -> Option<String> {
    match tv.call(Some(session), request).await {
        Response::Ok(Reply::NextEpisode { item }) => item.map(|i| i.id().to_string()),
        other => panic!("{other:?}"),
    }
}

async fn after(
    tv: &TestClient,
    session: &SessionId,
    key: u32,
    mode: NextMode,
    specials: bool,
) -> Option<String> {
    let request = Request::NextEpisode {
        item: episode(key),
        mode,
        specials,
    };
    found(tv, session, request).await
}

async fn show_next(tv: &TestClient, session: &SessionId) -> Option<String> {
    found(tv, session, Request::ShowNext { show: show() }).await
}

fn id(key: u32) -> Option<String> {
    Some(episode(key).to_string())
}

async fn mark(tv: &TestClient, session: &SessionId, key: u32) {
    let marked = tv
        .call(Some(session), Request::MarkWatched { id: episode(key) })
        .await;
    assert_eq!(marked, Response::Ok(Reply::Done));
}

/// In order: a season's last episode is followed by the next season's
/// first, the show's last by none. Specials are out of the order unless
/// asked for, and then first.
#[tokio::test]
async fn in_order_goes_across_seasons_and_ends_with_the_show() {
    let (server, _pms) = server("next-in-order").await;
    let (tv, alice) = alice(&server).await;

    assert_eq!(
        after(&tv, &alice, 211, NextMode::InOrder, false).await,
        id(212)
    );
    assert_eq!(
        after(&tv, &alice, 212, NextMode::InOrder, false).await,
        id(221)
    );
    assert_eq!(
        after(&tv, &alice, 222, NextMode::InOrder, false).await,
        None
    );
    assert_eq!(
        after(&tv, &alice, 201, NextMode::InOrder, false).await,
        None
    );
    assert_eq!(
        after(&tv, &alice, 201, NextMode::InOrder, true).await,
        id(211)
    );
    assert_eq!(after(&tv, &alice, 222, NextMode::InOrder, true).await, None);
    tv.close().await;
    server.stop().await;
}

/// First unwatched skips what the user watched, and is none when all
/// after the episode are.
#[tokio::test]
async fn first_unwatched_skips_what_was_watched() {
    let (server, _pms) = server("next-unwatched").await;
    let (tv, alice) = alice(&server).await;
    mark(&tv, &alice, 221).await;

    assert_eq!(
        after(&tv, &alice, 212, NextMode::FirstUnwatched, false).await,
        id(222)
    );
    assert_eq!(
        after(&tv, &alice, 212, NextMode::InOrder, false).await,
        id(221)
    );
    mark(&tv, &alice, 222).await;
    assert_eq!(
        after(&tv, &alice, 212, NextMode::FirstUnwatched, false).await,
        None
    );
    tv.close().await;
    server.stop().await;
}

/// A show's Continue: the first episode in progress, else the first not
/// watched, never a special; none once all are watched. The answer is
/// filled with the user's state.
#[tokio::test]
async fn show_next_is_in_progress_else_first_unwatched() {
    let (server, _pms) = server("show-next").await;
    let (tv, alice) = alice(&server).await;
    assert_eq!(show_next(&tv, &alice).await, id(211));
    mark(&tv, &alice, 211).await;
    assert_eq!(show_next(&tv, &alice).await, id(212));

    let user = server.store.list_users().await.unwrap()[0].id;
    let key = episode(222).to_string();
    let progress = WatchUpdate {
        media_id: &key,
        show_id: None,
        external_ids: &[],
        position_ms: Some(600_000),
        duration_ms: Some(1_200_000),
        watched: false,
        unhide: true,
    };
    server
        .store
        .save_watch(Principal::LocalUser(user), progress)
        .await
        .unwrap();
    let answered = tv
        .call(Some(&alice), Request::ShowNext { show: show() })
        .await;
    let Response::Ok(Reply::NextEpisode {
        item: Some(Item::Episode(next)),
    }) = answered
    else {
        panic!("{answered:?}");
    };
    assert_eq!(next.id, episode(222));
    assert_eq!(
        next.playback.position,
        Some(std::time::Duration::from_secs(600))
    );

    for key in [212, 221, 222] {
        mark(&tv, &alice, key).await;
    }
    assert_eq!(show_next(&tv, &alice).await, None);
    tv.close().await;
    server.stop().await;
}

/// The wrong kind of item is refused; a device that did not name the
/// feature is answered as one asking what the server does not know.
#[tokio::test]
async fn the_wrong_item_or_no_feature_is_refused() {
    let (server, _pms) = server("next-refused").await;
    let (tv, alice) = alice(&server).await;
    let not_an_episode = Request::NextEpisode {
        item: show(),
        mode: NextMode::InOrder,
        specials: false,
    };
    assert!(matches!(
        tv.call(Some(&alice), not_an_episode).await,
        Response::Err(Error::InvalidInput { .. })
    ));
    let not_a_show = Request::ShowNext { show: episode(211) };
    assert!(matches!(
        tv.call(Some(&alice), not_a_show).await,
        Response::Err(Error::InvalidInput { .. })
    ));

    let older = server.client().await;
    older.hello().await;
    let session = older.login("alice", "correct horse").await;
    assert_eq!(
        older
            .call(Some(&session), Request::ShowNext { show: show() })
            .await,
        Response::Err(Error::Unsupported {
            request: "show_next".into()
        })
    );
    older.close().await;
    tv.close().await;
    server.stop().await;
}

/// The SDK reaches the calls through the feature being on.
#[tokio::test]
async fn the_sdk_asks_through_the_feature() {
    let (server, _pms) = server("next-sdk").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = crate::sdk::client_on(&server, &storage).await;
    let alice = client
        .sign_in(
            &server.server.id().to_string(),
            "alice",
            "correct horse",
            false,
        )
        .await
        .unwrap();
    let next = alice.feature::<NextEpisodes>().unwrap();

    let after = next
        .next_episode(&episode(212), NextMode::InOrder, false)
        .await
        .unwrap();
    assert_eq!(after.map(|i| i.id().clone()), Some(episode(221)));
    let continued = next.show_next(&show()).await.unwrap();
    assert_eq!(continued.map(|i| i.id().clone()), Some(episode(211)));
    client.close().await;
    server.stop().await;
}

/// The app reaches the calls as an object got only with the feature on.
#[tokio::test]
async fn the_ffi_asks_through_the_feature() {
    let (server, _pms) = server("next-ffi").await;
    let storage = Arc::new(MemoryStorage::default());
    let client = crate::sdk::client_on(&server, &storage).await;
    let alice = client
        .sign_in(
            &server.server.id().to_string(),
            "alice",
            "correct horse",
            false,
        )
        .await
        .unwrap();
    let alice = SpoolSession::from(alice);
    assert!(alice.has_feature(Feature::NextEpisodes));
    let next = alice.next_episodes().unwrap();

    let after = next
        .next_episode(episode(212), NextMode::InOrder, false)
        .await
        .unwrap();
    assert_eq!(after.map(|i| i.id().clone()), Some(episode(221)));
    let continued = next.show_next(show()).await.unwrap();
    assert_eq!(continued.map(|i| i.id().clone()), Some(episode(211)));
    client.close().await;
    server.stop().await;
}

/// An app that did not ask for the feature gets no object for its calls.
#[tokio::test]
async fn the_ffi_has_no_next_episodes_with_the_feature_off() {
    let (server, _pms) = server("next-ffi-off").await;
    let id = server.server.id().to_string();
    let client = spoolframe_client_sdk::Client::open_with_options(
        Arc::new(MemoryStorage::default()),
        spoolframe_client_sdk::Device {
            name: "old tv".into(),
            client_name: "next-ffi-test".into(),
            client_version: "0".into(),
            platform: spoolframe_client_sdk::types::Platform::Tvos,
            capabilities: support::mkv_capabilities(),
        },
        spoolframe_client_sdk::ClientOptions {
            lan_discovery: false,
            features: Features::empty(),
            ..spoolframe_client_sdk::ClientOptions::default()
        },
    )
    .await
    .unwrap();
    client
        .add_address(&id, server.server.addr().ip_addrs().copied().collect())
        .unwrap();
    client.connect(&id).await.unwrap();
    client.pin(&id, true).await.unwrap();
    let alice = client
        .sign_in(&id, "alice", "correct horse", false)
        .await
        .unwrap();
    let alice = SpoolSession::from(alice);

    assert!(!alice.has_feature(Feature::NextEpisodes));
    assert!(alice.next_episodes().is_none());
    client.close().await;
    server.stop().await;
}
