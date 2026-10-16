//! Sharing a playlist by name (`spoolframe:playlist-sharing`) over a real
//! Iroh connection: whom a manager may share with, a page at a time and
//! by a query; the names of a playlist's owner and of who it is shared
//! with; and none of it for a client that did not list the feature.

use crate::support;

use spoolframe_protocol::features::{PLAYLIST_SHARING, PLAYLISTS};
use spoolframe_protocol::playlists::{
    Kinds, PlaylistShare, PlaylistSummary, ShareCandidatePage, UserKind, Visibility,
};
use spoolframe_protocol::{Error, Reply, Request, Response, SessionId};
use support::{TestClient, TestServer};

const EPISODES: Kinds = Kinds {
    movies: false,
    episodes: true,
    tracks: false,
};

/// A server with alice (shown as "Alice Liddell"), bob, carol and dave,
/// dave disabled, and a client that lists `features`.
async fn four_users(test: &str, features: &[&str]) -> (TestServer, TestClient) {
    let server = TestServer::start(test).await;
    server
        .add_user("alice", "correct horse", Some("Alice Liddell"))
        .await;
    server.add_user("bob", "correct horse", Some("Bob")).await;
    server.add_user("carol", "correct horse", None).await;
    server.add_user("dave", "correct horse", None).await;
    server.store.disable_user("dave").await.unwrap();
    let client = server.client().await;
    client.hello_listing(features).await;
    (server, client)
}

async fn user_id(client: &TestClient, session: &SessionId) -> String {
    match client.call(Some(session), Request::WhoAmI).await {
        Response::Ok(Reply::WhoAmI(who)) => who.user.user_id,
        other => panic!("{other:?}"),
    }
}

/// A playlist of `owner`'s, shared with `with`, read-only.
async fn shared(client: &TestClient, owner: &SessionId, with: Vec<String>) -> PlaylistSummary {
    let made = client
        .call(
            Some(owner),
            Request::CreatePlaylist {
                name: "Heists".into(),
                kinds: EPISODES,
                visibility: Visibility::Shared {
                    with: with
                        .into_iter()
                        .map(|id| PlaylistShare::new(id, false))
                        .collect(),
                },
            },
        )
        .await;
    match made {
        Response::Ok(Reply::PlaylistInfo(summary)) => summary,
        other => panic!("{other:?}"),
    }
}

async fn candidates(
    client: &TestClient,
    session: &SessionId,
    id: &str,
    query: &str,
    offset: u32,
    limit: u32,
) -> Response {
    client
        .call(
            Some(session),
            Request::ListShareCandidates {
                id: id.into(),
                query: query.into(),
                offset,
                limit,
            },
        )
        .await
}

fn page(response: Response) -> ShareCandidatePage {
    match response {
        Response::Ok(Reply::ShareCandidates(page)) => page,
        other => panic!("{other:?}"),
    }
}

fn names(page: &ShareCandidatePage) -> Vec<&str> {
    page.candidates
        .iter()
        .map(|c| c.display_name.as_str())
        .collect()
}

fn shares(summary: &PlaylistSummary) -> &[PlaylistShare] {
    match &summary.visibility {
        Visibility::Shared { with } => with,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_manager_lists_whom_to_share_with_by_name_and_not_themselves() {
    let (_server, tv) = four_users("sharing-candidates", &[PLAYLISTS, PLAYLIST_SHARING]).await;
    let alice = tv.login("alice", "correct horse").await;
    let mine = shared(&tv, &alice, Vec::new()).await;

    let everyone = page(candidates(&tv, &alice, &mine.id, "", 0, 50).await);
    // By name, in any case; not alice, not dave (disabled).
    assert_eq!(names(&everyone), ["Bob", "carol"]);
    assert_eq!(everyone.total, 2);
    let bob = &everyone.candidates[0];
    assert!(bob.user_id.starts_with("local:"), "{bob:?}");
    assert_eq!(bob.username.as_deref(), Some("bob"));
    assert_eq!(bob.kind, UserKind::Local);

    // A query is a substring of the name or the username, in any case.
    let by_name = page(candidates(&tv, &alice, &mine.id, "BO", 0, 50).await);
    assert_eq!(names(&by_name), ["Bob"]);
    let by_username = page(candidates(&tv, &alice, &mine.id, "aro", 0, 50).await);
    assert_eq!(names(&by_username), ["carol"]);
    let herself = page(candidates(&tv, &alice, &mine.id, "alice", 0, 50).await);
    assert!(herself.candidates.is_empty() && herself.total == 0);

    // A page at a time, the total counting them all.
    let second = page(candidates(&tv, &alice, &mine.id, "", 1, 1).await);
    assert_eq!(names(&second), ["carol"]);
    assert_eq!((second.offset, second.total), (1, 2));
}

#[tokio::test]
async fn whoever_does_not_manage_the_playlist_is_refused() {
    let (_server, tv) = four_users("sharing-refused", &[PLAYLISTS, PLAYLIST_SHARING]).await;
    let alice = tv.login("alice", "correct horse").await;
    let bob = tv.login("bob", "correct horse").await;
    let carol = tv.login("carol", "correct horse").await;
    let hers = shared(&tv, &alice, vec![user_id(&tv, &bob).await]).await;

    // Bob sees it, and may not share it: as for a rename.
    assert_eq!(
        candidates(&tv, &bob, &hers.id, "", 0, 50).await,
        Response::Err(Error::PlaylistReadOnly)
    );
    // Carol does not see it at all.
    assert!(matches!(
        candidates(&tv, &carol, &hers.id, "", 0, 50).await,
        Response::Err(Error::NotFound { .. })
    ));
}

#[tokio::test]
async fn shares_and_the_owner_carry_names() {
    let (server, tv) = four_users("sharing-names", &[PLAYLISTS, PLAYLIST_SHARING]).await;
    let alice = tv.login("alice", "correct horse").await;
    let bob = tv.login("bob", "correct horse").await;
    let bob_id = user_id(&tv, &bob).await;
    let dave_id = {
        let user = server.store.user_by_name("dave").await.unwrap().unwrap();
        format!("local:{}", user.id)
    };
    let hers = shared(&tv, &alice, vec![bob_id.clone(), dave_id.clone()]).await;
    assert_eq!(hers.owner_name.as_deref(), Some("Alice Liddell"));
    let named: Vec<_> = shares(&hers)
        .iter()
        .map(|s| (s.user_id.clone(), s.display_name.clone(), s.kind))
        .collect();
    // A disabled user is still named on a share they have.
    assert!(named.contains(&(bob_id, Some("Bob".into()), Some(UserKind::Local))));
    assert!(named.contains(&(dave_id, Some("dave".into()), Some(UserKind::Local))));

    // Bob's list says whose it is.
    let listed = match tv.call(Some(&bob), Request::ListPlaylists).await {
        Response::Ok(Reply::Playlists { playlists, .. }) => playlists,
        other => panic!("{other:?}"),
    };
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].owner_name.as_deref(), Some("Alice Liddell"));
    let opened = match tv
        .call(
            Some(&bob),
            Request::GetPlaylist {
                id: hers.id.clone(),
                offset: 0,
                limit: 10,
            },
        )
        .await
    {
        Response::Ok(Reply::Playlist(page)) => page,
        other => panic!("{other:?}"),
    };
    assert_eq!(opened.playlist.owner_name.as_deref(), Some("Alice Liddell"));
}

/// The features-over-version rule: an app built before
/// `spoolframe:playlist-sharing` gets no names, and its
/// `ListShareCandidates` is refused.
#[tokio::test]
async fn a_client_without_the_feature_gets_no_names_and_cannot_list_candidates() {
    let (_server, older) = four_users("sharing-older-client", &[PLAYLISTS]).await;
    let alice = older.login("alice", "correct horse").await;
    let bob = older.login("bob", "correct horse").await;
    let hers = shared(&older, &alice, vec![user_id(&older, &bob).await]).await;
    assert_eq!(hers.owner_name, None);
    assert!(
        shares(&hers)
            .iter()
            .all(|s| s.display_name.is_none() && s.kind.is_none()),
        "{hers:?}"
    );
    let listed = match older.call(Some(&bob), Request::ListPlaylists).await {
        Response::Ok(Reply::Playlists { playlists, .. }) => playlists,
        other => panic!("{other:?}"),
    };
    assert_eq!(listed[0].owner_name, None);
    assert!(matches!(
        candidates(&older, &alice, &hers.id, "", 0, 50).await,
        Response::Err(Error::Unsupported { .. })
    ));
}
