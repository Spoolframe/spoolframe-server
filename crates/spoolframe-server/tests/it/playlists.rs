//! Playlists (PX-40) over a real Iroh connection: that the requests reach
//! `Playlists`, their refusals reach the client as typed errors, and the
//! changes reach the devices concerned as events. The rules themselves
//! are tested in `playlists::tests` and `store::playlists::tests`.

use crate::support;

use std::time::Duration;

use iroh::endpoint::RecvStream;
use spoolframe_protocol::codec::read_frame;
use spoolframe_protocol::features::{PLAYLIST_UPCOMING, PLAYLISTS};
use spoolframe_protocol::playlists::{
    ItemKind, Kinds, PinNoteReason, PlaylistEdit, PlaylistOwnerKind, PlaylistShare,
    PlaylistSummary, Visibility,
};
use spoolframe_protocol::streams::uni;
use spoolframe_protocol::{Error, Event, Reply, Request, Response, SessionId};
use spoolframe_types::{MediaId, MediaKind, Namespace, Role};
use support::{TestClient, TestServer};
use tokio::io::AsyncReadExt;

const EPISODES: Kinds = Kinds {
    movies: false,
    episodes: true,
    tracks: false,
};

fn episode(key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, MediaKind::Episode, key).unwrap()
}

fn season(key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, MediaKind::Season, key).unwrap()
}

/// A server with alice, bob and carol, and a client that lists the
/// playlists feature.
async fn three_users(test: &str) -> (TestServer, TestClient) {
    let server = TestServer::start(test).await;
    for name in ["alice", "bob", "carol"] {
        server.add_user(name, "correct horse", None).await;
    }
    let tv = server.client().await;
    tv.hello_listing(&[PLAYLISTS]).await;
    (server, tv)
}

async fn user_id(client: &TestClient, session: &SessionId) -> String {
    match client.call(Some(session), Request::WhoAmI).await {
        Response::Ok(Reply::WhoAmI(who)) => who.user.user_id,
        other => panic!("{other:?}"),
    }
}

async fn create(
    client: &TestClient,
    session: &SessionId,
    kinds: Kinds,
    visibility: Visibility,
) -> PlaylistSummary {
    let made = client
        .call(
            Some(session),
            Request::CreatePlaylist {
                name: "Heists".into(),
                kinds,
                visibility,
            },
        )
        .await;
    match made {
        Response::Ok(Reply::PlaylistInfo(summary)) => summary,
        other => panic!("{other:?}"),
    }
}

async fn edit(
    client: &TestClient,
    session: &SessionId,
    id: &str,
    revision: u64,
    edits: Vec<PlaylistEdit>,
) -> Response {
    client
        .call(
            Some(session),
            Request::EditPlaylist {
                id: id.into(),
                revision,
                edits,
            },
        )
        .await
}

fn add(id: MediaId) -> PlaylistEdit {
    PlaylistEdit::Add { id, before: None }
}

async fn list(client: &TestClient, session: &SessionId) -> Reply {
    match client.call(Some(session), Request::ListPlaylists).await {
        Response::Ok(reply @ Reply::Playlists { .. }) => reply,
        other => panic!("{other:?}"),
    }
}

async fn events(client: &TestClient) -> RecvStream {
    let mut events = tokio::time::timeout(Duration::from_secs(5), client.conn.accept_uni())
        .await
        .expect("no events stream within 5 s")
        .unwrap();
    assert_eq!(events.read_u8().await.unwrap(), uni::EVENTS);
    events
}

/// The next event on `events` that is not the server's addresses.
async fn next_event(events: &mut RecvStream) -> Event {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), read_frame::<_, Event>(events))
            .await
            .expect("no event within 5 s")
            .unwrap();
        if !matches!(event, Event::ServerAddresses { .. }) {
            return event;
        }
    }
}

#[tokio::test]
async fn a_playlist_is_made_filled_read_and_refused_over_the_wire() {
    let (server, tv) = three_users("playlists-wire").await;
    server.write_episode("12");
    server.write_episode("13");
    let alice = tv.login("alice", "correct horse").await;
    let bob = tv.login("bob", "correct horse").await;
    let carol = tv.login("carol", "correct horse").await;
    let shares = vec![PlaylistShare::new(user_id(&tv, &bob).await, false)];
    let made = create(&tv, &alice, EPISODES, Visibility::Shared { with: shares }).await;

    // An episodes playlist takes no film.
    let film = edit(
        &tv,
        &alice,
        &made.id,
        0,
        vec![add(support::movie().id().clone())],
    )
    .await;
    assert_eq!(
        film,
        Response::Err(Error::KindNotAllowed {
            kind: ItemKind::Movie
        })
    );
    let Response::Ok(Reply::PlaylistEdited { revision }) =
        edit(&tv, &alice, &made.id, 0, vec![add(season("11"))]).await
    else {
        panic!("the season was not added");
    };

    let page = tv
        .call(
            Some(&bob),
            Request::GetPlaylist {
                id: made.id.clone(),
                offset: 0,
                limit: 10,
            },
        )
        .await;
    let Response::Ok(Reply::Playlist(page)) = page else {
        panic!("{page:?}");
    };
    let items: Vec<_> = page.entries.iter().map(|e| e.item.id().clone()).collect();
    assert_eq!(items, [episode("12"), episode("13")]);
    assert_eq!((page.total, page.playable), (2, 2));
    assert_eq!(page.playlist.owner_kind, PlaylistOwnerKind::SharedWithMe);

    // Read only for Bob; not there at all for Carol.
    assert_eq!(
        edit(&tv, &bob, &made.id, revision, vec![add(episode("15"))]).await,
        Response::Err(Error::PlaylistReadOnly)
    );
    assert!(matches!(
        tv.call(
            Some(&carol),
            Request::GetPlaylist {
                id: made.id.clone(),
                offset: 0,
                limit: 10,
            },
        )
        .await,
        Response::Err(Error::NotFound { .. })
    ));

    // Two editors at one revision: the first removes an entry, the
    // second's move of it is refused, with the revision to reload at.
    let first = page.entries[0].entry;
    let Response::Ok(Reply::PlaylistEdited { revision: after }) = edit(
        &tv,
        &alice,
        &made.id,
        revision,
        vec![PlaylistEdit::Remove { entry: first }],
    )
    .await
    else {
        panic!("not removed");
    };
    assert_eq!(
        edit(
            &tv,
            &alice,
            &made.id,
            revision,
            vec![PlaylistEdit::Move {
                entry: first,
                before: None
            }],
        )
        .await,
        Response::Err(Error::PlaylistConflict { revision: after })
    );
}

/// The ticket's last check: a client without the feature shows no
/// playlists, as the server gives it none on an item; a client with it
/// gets the item's playlists, and the session's whole feature list.
#[tokio::test]
async fn a_client_without_the_feature_gets_no_playlists_on_an_item() {
    let (server, tv) = three_users("playlists-feature").await;
    let alice = tv.login("alice", "correct horse").await;
    let made = create(&tv, &alice, EPISODES, Visibility::Personal).await;
    edit(&tv, &alice, &made.id, 0, vec![add(episode("12"))]).await;
    let show = MediaId::new(Namespace::Plex, MediaKind::Show, "10").unwrap();
    let Response::Ok(Reply::Item { playlists, .. }) = tv
        .call(Some(&alice), Request::Item { id: show.clone() })
        .await
    else {
        panic!("no item");
    };
    let playlists = playlists.expect("a client with the feature gets them");
    assert_eq!(playlists.len(), 1);
    assert_eq!(playlists[0].id, made.id);

    let older = server.client().await;
    let info = older.hello_listing(&[]).await;
    // No playlists before a session: they are not pre-auth.
    assert!(info.features.is_empty());
    let request = Request::LoginPassword(spoolframe_protocol::LoginPassword {
        username: "alice".into(),
        password: "correct horse".into(),
        remember: false,
    });
    let Response::Ok(Reply::LoggedIn {
        session, features, ..
    }) = older.call(None, request).await
    else {
        panic!("not signed in");
    };
    assert!(features.iter().any(|f| f == PLAYLISTS), "{features:?}");
    let Response::Ok(Reply::Item { playlists, .. }) =
        older.call(Some(&session), Request::Item { id: show }).await
    else {
        panic!("no item");
    };
    assert_eq!(playlists, None);
}

/// The ticket's pins: pinning on one device shows the pin, in its order,
/// on another; a pinned playlist unshared from the user leaves Pinned
/// with a note, and their devices are told.
#[tokio::test]
async fn a_pin_shows_on_another_device_and_an_unshare_takes_it_away_with_a_note() {
    let (server, tv) = three_users("playlists-pins").await;
    let alice = tv.login("alice", "correct horse").await;
    let phone = server.client().await;
    phone.hello_listing(&[PLAYLISTS]).await;
    let mut phone_events = events(&phone).await;
    let laptop = server.client().await;
    laptop.hello_listing(&[PLAYLISTS]).await;
    let mut laptop_events = events(&laptop).await;
    let bob_on_phone = phone.login("bob", "correct horse").await;
    let bob_on_laptop = laptop.login("bob", "correct horse").await;
    let bob_id = user_id(&phone, &bob_on_phone).await;
    // A device is told of changes from its first request as the user, as
    // an app makes when it shows its playlists.
    list(&laptop, &bob_on_laptop).await;
    let shared = Visibility::Shared {
        with: vec![PlaylistShare::new(bob_id, true)],
    };
    let first = create(&tv, &alice, EPISODES, shared.clone()).await;
    let second = create(&tv, &alice, EPISODES, shared).await;
    // Bob's laptop is told of each playlist shared with him.
    assert!(matches!(
        next_event(&mut laptop_events).await,
        Event::PlaylistChanged { id, revision: 0 } if id == first.id
    ));
    assert!(matches!(
        next_event(&mut laptop_events).await,
        Event::PlaylistChanged { id, .. } if id == second.id
    ));

    for (id, position) in [(&first.id, 0), (&second.id, 0)] {
        let pinned = phone
            .call(
                Some(&bob_on_phone),
                Request::PinPlaylist {
                    id: id.clone(),
                    position,
                },
            )
            .await;
        assert_eq!(pinned, Response::Ok(Reply::Done));
    }
    assert_eq!(next_event(&mut laptop_events).await, Event::PinsChanged);
    assert_eq!(next_event(&mut laptop_events).await, Event::PinsChanged);
    let Reply::Playlists { playlists, .. } = list(&laptop, &bob_on_laptop).await else {
        unreachable!()
    };
    let pinned: Vec<_> = playlists.iter().map(|p| (p.id.clone(), p.pinned)).collect();
    assert_eq!(
        pinned,
        [(first.id.clone(), Some(1)), (second.id.clone(), Some(0))]
    );

    // Alice unshares the first.
    let unshared = tv
        .call(
            Some(&alice),
            Request::UpdatePlaylist {
                id: first.id.clone(),
                name: None,
                kinds: None,
                visibility: Some(Visibility::Personal),
            },
        )
        .await;
    assert!(matches!(unshared, Response::Ok(Reply::PlaylistInfo(_))));
    // Past what the phone was told before: the makes and its own pins.
    loop {
        match next_event(&mut phone_events).await {
            Event::PlaylistGone { id } => {
                assert_eq!(id, first.id);
                break;
            }
            Event::PlaylistChanged { .. } | Event::PinsChanged => {}
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(next_event(&mut phone_events).await, Event::PinsChanged);
    let Reply::Playlists {
        playlists,
        pin_notes,
    } = list(&laptop, &bob_on_laptop).await
    else {
        unreachable!()
    };
    assert_eq!(playlists.len(), 1);
    assert_eq!(playlists[0].pinned, Some(0));
    assert_eq!(pin_notes.len(), 1);
    assert_eq!(pin_notes[0].reason, PinNoteReason::Unshared);
    // Dismissed on one device, gone on the other.
    assert_eq!(
        phone
            .call(Some(&bob_on_phone), Request::DismissPinNotes)
            .await,
        Response::Ok(Reply::Done)
    );
    let Reply::Playlists { pin_notes, .. } = list(&laptop, &bob_on_laptop).await else {
        unreachable!()
    };
    assert!(pin_notes.is_empty());
}

#[tokio::test]
async fn a_server_wide_playlist_is_an_admin_s_and_seen_by_everyone() {
    let (server, tv) = three_users("playlists-server-wide").await;
    server.store.set_role("carol", Role::Admin).await.unwrap();
    let alice = tv.login("alice", "correct horse").await;
    let carol = tv.login("carol", "correct horse").await;
    let refused = tv
        .call(
            Some(&alice),
            Request::CreatePlaylist {
                name: "Ours".into(),
                kinds: EPISODES,
                visibility: Visibility::ServerWide,
            },
        )
        .await;
    assert_eq!(refused, Response::Err(Error::AdminRequired));
    let made = create(&tv, &carol, EPISODES, Visibility::ServerWide).await;
    let Reply::Playlists { playlists, .. } = list(&tv, &alice).await else {
        unreachable!()
    };
    assert_eq!(playlists.len(), 1);
    assert_eq!(playlists[0].id, made.id);
    assert_eq!(playlists[0].owner_kind, PlaylistOwnerKind::ServerWide);
    assert!(!playlists[0].can_edit);
    assert_eq!(
        tv.call(
            Some(&alice),
            Request::DeletePlaylist {
                id: made.id.clone()
            }
        )
        .await,
        Response::Err(Error::PlaylistReadOnly)
    );
}

#[tokio::test]
async fn a_playlist_plays_over_the_wire_to_its_end() {
    let (server, tv) = three_users("playlists-play").await;
    server.write_media(1000);
    for key in ["12", "13"] {
        server.write_episode(key);
    }
    let alice = tv.login("alice", "correct horse").await;
    let made = create(&tv, &alice, EPISODES, Visibility::Personal).await;
    edit(&tv, &alice, &made.id, 0, vec![add(season("11"))]).await;
    let play = Request::PlayPlaylist {
        id: made.id.clone(),
        from: None,
        shuffle: false,
        repeat: spoolframe_protocol::playlists::Repeat::Off,
        skip_watched: false,
    };
    let Response::Ok(Reply::PlaylistItem(first)) = tv.call(Some(&alice), play).await else {
        panic!("nothing to play");
    };
    assert_eq!(first.item.id(), &episode("12"));
    let next = |after| Request::NextInPlaylist {
        id: made.id.clone(),
        after,
    };
    let Response::Ok(Reply::PlaylistItem(second)) = tv.call(Some(&alice), next(first.entry)).await
    else {
        panic!("no second");
    };
    assert_eq!(second.item.id(), &episode("13"));
    assert_eq!(
        tv.call(Some(&alice), next(second.entry)).await,
        Response::Ok(Reply::PlaylistFinished)
    );
}

/// `spoolframe:playlist-upcoming`: what comes up after the place, and the
/// settings saved alone, which keep the place and change what comes up.
#[tokio::test]
async fn upcoming_and_settings_alone_reach_the_server_over_the_wire() {
    let server = TestServer::start("playlists-upcoming").await;
    server.add_user("alice", "correct horse", None).await;
    let tv = server.client().await;
    tv.hello_listing(&[PLAYLISTS, PLAYLIST_UPCOMING]).await;
    server.write_media(1000);
    for key in ["12", "13"] {
        server.write_episode(key);
    }
    let alice = tv.login("alice", "correct horse").await;
    let made = create(&tv, &alice, EPISODES, Visibility::Personal).await;
    edit(&tv, &alice, &made.id, 0, vec![add(season("11"))]).await;
    let play = Request::PlayPlaylist {
        id: made.id.clone(),
        from: None,
        shuffle: false,
        repeat: spoolframe_protocol::playlists::Repeat::Off,
        skip_watched: false,
    };
    let Response::Ok(Reply::PlaylistItem(first)) = tv.call(Some(&alice), play).await else {
        panic!("nothing to play");
    };
    let set = Request::SetPlaylistSettings {
        id: made.id.clone(),
        shuffle: false,
        repeat: spoolframe_protocol::playlists::Repeat::All,
        skip_watched: false,
    };
    assert_eq!(tv.call(Some(&alice), set).await, Response::Ok(Reply::Done));
    let upcoming = Request::UpcomingInPlaylist {
        id: made.id.clone(),
        limit: 3,
    };
    let Response::Ok(Reply::PlaylistUpcoming { entries }) = tv.call(Some(&alice), upcoming).await
    else {
        panic!("no upcoming");
    };
    let keys: Vec<&str> = entries.iter().map(|e| e.item.id().key()).collect();
    // After 12, which plays on: 13, then round again with repeat all.
    assert_eq!(keys, ["13", "12", "13"], "after {:?}", first.item.id());
}

/// A client that did not list the feature gets neither request.
#[tokio::test]
async fn a_client_without_playlist_upcoming_is_refused_both_requests() {
    let (_server, tv) = three_users("playlists-upcoming-unlisted").await;
    let alice = tv.login("alice", "correct horse").await;
    let made = create(&tv, &alice, EPISODES, Visibility::Personal).await;
    for request in [
        Request::UpcomingInPlaylist {
            id: made.id.clone(),
            limit: 3,
        },
        Request::SetPlaylistSettings {
            id: made.id.clone(),
            shuffle: true,
            repeat: spoolframe_protocol::playlists::Repeat::Off,
            skip_watched: false,
        },
    ] {
        let answer = tv.call(Some(&alice), request).await;
        assert!(
            matches!(answer, Response::Err(Error::Unsupported { .. })),
            "{answer:?}"
        );
    }
}
