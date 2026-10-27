//! A library sorted and filtered over a real Iroh connection: what a
//! library does not offer refused; each user's own watch state; and a
//! plain `Items` as it always was. The sorting and filtering themselves
//! are tested in `watch::browse`, the PMS's in spoolframe-plex and the
//! cache's in the store.

use crate::support;

use spoolframe_protocol::{Error, Hello, PROTOCOL_VERSION, Reply, Request, Response, SessionId};
use spoolframe_types::{FilterPart, Item, ItemFilter, ItemSort, LibraryId, SortKey, WatchedState};
use support::{TestClient, TestServer};

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
        device_name: Some("Living room".into()),
        capabilities: Some(capabilities),
        experimental: Vec::new(),
    };
    assert!(matches!(
        client.call(None, Request::Hello(hello)).await,
        Response::Ok(Reply::ServerInfo(_))
    ));
    client
}

fn movies() -> LibraryId {
    support::library().id
}

fn items(sort: Option<ItemSort>, filter: Option<ItemFilter>) -> Request {
    Request::Items {
        library: movies(),
        offset: 0,
        limit: 50,
        sort,
        filter,
    }
}

fn watched(state: WatchedState) -> Option<ItemFilter> {
    Some(ItemFilter {
        watched: vec![state],
        ..ItemFilter::default()
    })
}

async fn listed(tv: &TestClient, who: &SessionId, request: Request) -> Vec<String> {
    match tv.call(Some(who), request).await {
        Response::Ok(Reply::Items { items, total, .. }) => {
            assert_eq!(total as usize, items.len());
            items.iter().map(|i: &Item| i.id().to_string()).collect()
        }
        other => panic!("no page: {other:?}"),
    }
}

#[tokio::test]
async fn a_page_without_a_sort_or_filter_is_as_it_always_was() {
    let server = TestServer::start("sort-filter-plain").await;
    server.add_user("alice", "correct horse", None).await;
    let tv = device(&server).await;
    let alice = tv.login("alice", "correct horse").await;
    assert_eq!(
        listed(&tv, &alice, items(None, None)).await,
        [support::movie().id().to_string()]
    );
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn a_library_says_what_it_offers_and_refuses_the_rest() {
    let server = TestServer::start("sort-filter-offers").await;
    server.add_user("alice", "correct horse", None).await;
    let tv = device(&server).await;
    let alice = tv.login("alice", "correct horse").await;
    let values = match tv
        .call(
            Some(&alice),
            Request::ItemFilterValues { library: movies() },
        )
        .await
    {
        Response::Ok(Reply::ItemFilterValues { values }) => values,
        other => panic!("no values: {other:?}"),
    };
    assert_eq!(values.sorts, vec![SortKey::Title, SortKey::LastPlayed]);
    assert_eq!(values.filters, vec![FilterPart::Watched]);
    let invalid = |r: Response| matches!(r, Response::Err(Error::InvalidInput { .. }));
    let size = ItemSort {
        key: SortKey::Size,
        descending: true,
    };
    assert!(invalid(
        tv.call(Some(&alice), items(Some(size), None)).await
    ));
    let genre = ItemFilter {
        genres: vec!["Drama".into()],
        ..ItemFilter::default()
    };
    assert!(invalid(
        tv.call(Some(&alice), items(None, Some(genre))).await
    ));
    let unknown = ItemSort {
        key: SortKey::Unknown,
        descending: false,
    };
    assert!(invalid(
        tv.call(Some(&alice), items(Some(unknown), None)).await
    ));
    // A library not served is not found, as its `Items` is.
    let gone = LibraryId::new(spoolframe_types::Namespace::Plex, "99").unwrap();
    assert!(matches!(
        tv.call(Some(&alice), Request::ItemFilterValues { library: gone })
            .await,
        Response::Err(Error::NotFound { .. })
    ));
    tv.close().await;
    server.stop().await;
}

#[tokio::test]
async fn the_watch_state_filter_is_the_session_user_s() {
    let server = TestServer::start("sort-filter-own").await;
    server.add_user("alice", "correct horse", None).await;
    server.add_user("bob", "battery staple", None).await;
    let tv = device(&server).await;
    let alice = tv.login("alice", "correct horse").await;
    let bob = tv.login("bob", "battery staple").await;
    let film = support::movie().id().clone();
    crate::watch::progress(&server, &tv, &alice, &film, 600).await;
    let film = film.to_string();
    assert_eq!(
        listed(&tv, &alice, items(None, watched(WatchedState::InProgress))).await,
        std::slice::from_ref(&film)
    );
    assert!(
        listed(&tv, &alice, items(None, watched(WatchedState::Unwatched)))
            .await
            .is_empty()
    );
    assert_eq!(
        listed(&tv, &bob, items(None, watched(WatchedState::Unwatched))).await,
        std::slice::from_ref(&film)
    );
    // And the page carries the user's state, as a plain one does.
    match tv
        .call(Some(&alice), items(None, watched(WatchedState::InProgress)))
        .await
    {
        Response::Ok(Reply::Items { items, .. }) => {
            let Item::Movie(m) = &items[0] else {
                panic!("not a movie")
            };
            assert!(m.playback.position.is_some());
        }
        other => panic!("no page: {other:?}"),
    }
    let last = ItemSort {
        key: SortKey::LastPlayed,
        descending: true,
    };
    assert_eq!(listed(&tv, &alice, items(Some(last), None)).await, [film]);
    tv.close().await;
    server.stop().await;
}
