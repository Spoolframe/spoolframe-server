//! Liked and All (PX-43) over a real Iroh connection: `Like` and `Unlike`
//! reach `Playlists`, an item's details carry its hearts, and all of it,
//! the built-in playlists too, only for a client that listed
//! `spoolframe:liked`. The rules are tested in `playlists::builtin_tests`.

use crate::support;

use spoolframe_protocol::features::{LIKED, PLAYLISTS};
use spoolframe_protocol::playlists::{Builtin, ItemKind, Liked, PlaylistSummary};
use spoolframe_protocol::{Error, Reply, Request, Response, SessionId};
use spoolframe_types::{MediaId, MediaKind, Namespace};
use support::{TestClient, TestServer};

fn id(kind: MediaKind, key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, kind, key).unwrap()
}

/// A server whose fake catalog serves its music, with alice, and a client
/// that lists `features`, signed in as her.
async fn alice_listing(test: &str, features: &[&str]) -> (TestServer, TestClient, SessionId) {
    let server = TestServer::start(test).await;
    server.catalog.serve_music();
    server.add_user("alice", "correct horse", None).await;
    let client = server.client().await;
    client.hello_listing(features).await;
    let session = client.login("alice", "correct horse").await;
    (server, client, session)
}

async fn playlists(client: &TestClient, session: &SessionId) -> Vec<PlaylistSummary> {
    match client.call(Some(session), Request::ListPlaylists).await {
        Response::Ok(Reply::Playlists { playlists, .. }) => playlists,
        other => panic!("{other:?}"),
    }
}

/// An item's `liked` and `liked_children`, and the playlists holding it.
async fn hearts(
    client: &TestClient,
    session: &SessionId,
    item: MediaId,
) -> (Option<Liked>, Option<Vec<MediaId>>, Vec<Option<Builtin>>) {
    match client.call(Some(session), Request::Item { id: item }).await {
        Response::Ok(Reply::Item {
            liked,
            liked_children,
            playlists,
            ..
        }) => (
            liked,
            liked_children,
            playlists
                .unwrap_or_default()
                .iter()
                .map(|p| p.builtin)
                .collect(),
        ),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_client_with_the_feature_likes_an_album_and_sees_its_hearts() {
    let (_server, tv, alice) = alice_listing("liked-wire", &[PLAYLISTS, LIKED]).await;
    let listed = playlists(&tv, &alice).await;
    let builtins: Vec<_> = listed.iter().map(|p| p.builtin).collect();
    assert_eq!(builtins, [Some(Builtin::Liked), Some(Builtin::All)]);
    assert!(!listed[0].can_manage && listed[0].can_edit);
    assert!(!listed[1].can_manage && !listed[1].can_edit);

    let album = id(MediaKind::Album, "31");
    let liked = match tv
        .call(
            Some(&alice),
            Request::Like {
                items: vec![album.clone()],
            },
        )
        .await
    {
        Response::Ok(Reply::PlaylistInfo(liked)) => liked,
        other => panic!("{other:?}"),
    };
    assert_eq!(liked.builtin, Some(Builtin::Liked));
    let (heart, children, holding) = hearts(&tv, &alice, album.clone()).await;
    assert_eq!(heart, Some(Liked::All));
    assert_eq!(children.map(|c| c.len()), Some(2));
    assert_eq!(holding, [Some(Builtin::Liked)]);
    let page = match tv
        .call(
            Some(&alice),
            Request::GetPlaylist {
                id: liked.id.clone(),
                offset: 0,
                limit: 10,
            },
        )
        .await
    {
        Response::Ok(Reply::Playlist(page)) => page,
        other => panic!("{other:?}"),
    };
    assert_eq!(page.total, 2);
    assert!(page.entries.iter().all(|e| e.liked));

    tv.call(
        Some(&alice),
        Request::Unlike {
            items: vec![id(MediaKind::Track, "33")],
        },
    )
    .await;
    assert_eq!(hearts(&tv, &alice, album).await.0, Some(Liked::Some));
    let (track, _, _) = hearts(&tv, &alice, id(MediaKind::Track, "32")).await;
    assert_eq!(track, Some(Liked::All));
    // A film has no heart, and is not liked.
    let film = id(MediaKind::Movie, "1");
    assert_eq!(hearts(&tv, &alice, film.clone()).await.0, None);
    assert_eq!(
        tv.call(Some(&alice), Request::Like { items: vec![film] })
            .await,
        Response::Err(Error::KindNotAllowed {
            kind: ItemKind::Movie
        })
    );
}

/// The features-over-version rule: an app built before `spoolframe:liked`
/// sees neither Liked nor All, nor a heart, and its `Like` is refused,
/// even when the user liked something on a newer one.
#[tokio::test]
async fn a_client_without_the_feature_sees_no_builtins_or_hearts_and_cannot_like() {
    let (server, newer, alice) = alice_listing("liked-older-client", &[PLAYLISTS, LIKED]).await;
    let album = id(MediaKind::Album, "31");
    let liked = match newer
        .call(
            Some(&alice),
            Request::Like {
                items: vec![album.clone()],
            },
        )
        .await
    {
        Response::Ok(Reply::PlaylistInfo(liked)) => liked,
        other => panic!("{other:?}"),
    };

    let older = server.client().await;
    older.hello_listing(&[PLAYLISTS]).await;
    let session = older.login("alice", "correct horse").await;
    assert!(playlists(&older, &session).await.is_empty());
    let (heart, children, holding) = hearts(&older, &session, album.clone()).await;
    assert_eq!((heart, children), (None, None));
    assert!(holding.is_empty(), "{holding:?}");
    assert!(matches!(
        older
            .call(Some(&session), Request::Like { items: vec![album] })
            .await,
        Response::Err(Error::Unsupported { .. })
    ));
    let opened = older
        .call(
            Some(&session),
            Request::GetPlaylist {
                id: liked.id,
                offset: 0,
                limit: 10,
            },
        )
        .await;
    assert!(
        matches!(opened, Response::Err(Error::NotFound { .. })),
        "{opened:?}"
    );
}
