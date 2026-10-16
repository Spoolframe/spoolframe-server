use super::*;
use crate::store::tests::{TestStore, test_store};

const ALICE: &str = "local:00000000-0000-0000-0000-000000000001";
const BOB: &str = "local:00000000-0000-0000-0000-000000000002";
const CAROL: &str = "local:00000000-0000-0000-0000-000000000003";

const EPISODES: Kinds = Kinds {
    movies: false,
    episodes: true,
    tracks: false,
};

fn episode(n: u32) -> NewEntry {
    NewEntry {
        media_id: format!("plex:episode:{n}"),
        kind: "episode".into(),
        show_id: Some("plex:show:1".into()),
        album_id: None,
        artist_id: None,
        external_ids: vec![format!("tvdb://{n}")],
    }
}

async fn playlist(store: &Store, owner: &str, shares: Vec<ShareRow>) -> PlaylistRow {
    store
        .create_playlist(&NewPlaylist {
            owner: owner.into(),
            name: "Heists".into(),
            kinds: EPISODES,
            server_wide: false,
            shares,
        })
        .await
        .unwrap()
}

fn ids(rows: &[PlaylistRow]) -> Vec<&str> {
    rows.iter().map(|r| r.id.as_str()).collect()
}

async fn order(store: &Store, id: &str) -> Vec<String> {
    store
        .playlist_entries(id)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.media_id)
        .collect()
}

async fn add(store: &Store, id: &str, base: u64, n: u32) -> Result<u64, PlaylistError> {
    store
        .edit_playlist(
            id,
            base,
            vec![EntryEdit::Add {
                entries: vec![episode(n)],
                before: None,
            }],
        )
        .await
}

async fn store(test: &str) -> TestStore {
    test_store(test).await
}

#[tokio::test]
async fn a_playlist_is_seen_by_its_owner_those_shared_with_and_no_one_else() {
    let store = store("playlists-seen").await;
    let personal = playlist(&store, ALICE, vec![]).await;
    let shared = playlist(
        &store,
        ALICE,
        vec![ShareRow {
            principal: BOB.into(),
            can_edit: false,
        }],
    )
    .await;
    let everyone = store
        .create_playlist(&NewPlaylist {
            server_wide: true,
            ..NewPlaylist {
                owner: CAROL.into(),
                name: "Specials".into(),
                kinds: EPISODES,
                server_wide: false,
                shares: vec![],
            }
        })
        .await
        .unwrap();
    let alice = store.playlists_seen_by(ALICE).await.unwrap();
    assert_eq!(ids(&alice), [&personal.id, &shared.id, &everyone.id]);
    let bob = store.playlists_seen_by(BOB).await.unwrap();
    assert_eq!(ids(&bob), [&shared.id, &everyone.id]);
    assert_eq!(bob[0].share_of(BOB), Some(false));
    let stranger = "portable:".to_owned() + &"ab".repeat(32);
    let guest = store.playlists_seen_by(&stranger).await.unwrap();
    assert_eq!(ids(&guest), [&everyone.id]);
}

#[tokio::test]
async fn edits_keep_their_order_and_raise_the_revision() {
    let store = store("playlists-order").await;
    let list = playlist(&store, ALICE, vec![]).await;
    let r1 = add(&store, &list.id, 0, 1).await.unwrap();
    let r2 = add(&store, &list.id, r1, 2).await.unwrap();
    assert_eq!((r1, r2), (1, 2));
    let entries = store.playlist_entries(&list.id).await.unwrap();
    // An item may be in a playlist twice, as two entries.
    let r3 = store
        .edit_playlist(
            &list.id,
            r2,
            vec![
                EntryEdit::Add {
                    entries: vec![episode(2)],
                    before: Some(entries[0].entry_id),
                },
                EntryEdit::Move {
                    entry: entries[0].entry_id,
                    before: None,
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(r3, 3);
    assert_eq!(
        order(&store, &list.id).await,
        ["plex:episode:2", "plex:episode:2", "plex:episode:1"]
    );
}

/// The ticket's two editors at one revision: the first removes an entry;
/// the second's add is redone on the new revision, its move of the entry
/// the first removed refused.
#[tokio::test]
async fn a_second_editor_s_add_is_redone_and_a_move_of_a_removed_entry_refused() {
    let store = store("playlists-two-editors").await;
    let list = playlist(&store, ALICE, vec![]).await;
    let mut revision = 0;
    for n in 1..=3 {
        revision = add(&store, &list.id, revision, n).await.unwrap();
    }
    let seen = revision;
    let first = store.playlist_entries(&list.id).await.unwrap()[0].entry_id;
    store
        .edit_playlist(&list.id, seen, vec![EntryEdit::Remove(first)])
        .await
        .unwrap();

    add(&store, &list.id, seen, 4).await.unwrap();
    assert_eq!(
        order(&store, &list.id).await,
        ["plex:episode:2", "plex:episode:3", "plex:episode:4"]
    );
    let moved = store
        .edit_playlist(
            &list.id,
            seen,
            vec![EntryEdit::Move {
                entry: first,
                before: None,
            }],
        )
        .await;
    assert!(matches!(moved, Err(PlaylistError::Conflict)), "{moved:?}");
}

#[tokio::test]
async fn a_refused_edit_changes_nothing() {
    let store = store("playlists-refused").await;
    let list = playlist(&store, ALICE, vec![]).await;
    let r1 = add(&store, &list.id, 0, 1).await.unwrap();
    // An add, then a remove of an entry that is not there: all or nothing.
    let refused = store
        .edit_playlist(
            &list.id,
            r1,
            vec![
                EntryEdit::Add {
                    entries: vec![episode(2)],
                    before: None,
                },
                EntryEdit::Remove(9_999),
            ],
        )
        .await;
    assert!(matches!(refused, Err(PlaylistError::Conflict)));
    assert_eq!(order(&store, &list.id).await, ["plex:episode:1"]);
    assert_eq!(
        store.playlist(&list.id).await.unwrap().unwrap().revision,
        r1
    );
}

#[tokio::test]
async fn an_entry_id_is_never_handed_out_again() {
    let store = store("playlists-entry-ids").await;
    let list = playlist(&store, ALICE, vec![]).await;
    let r1 = add(&store, &list.id, 0, 1).await.unwrap();
    let first = store.playlist_entries(&list.id).await.unwrap()[0].entry_id;
    let r2 = store
        .edit_playlist(&list.id, r1, vec![EntryEdit::Remove(first)])
        .await
        .unwrap();
    add(&store, &list.id, r2, 1).await.unwrap();
    let again = store.playlist_entries(&list.id).await.unwrap()[0].entry_id;
    assert_ne!(again, first);
}

/// An item that left the library is dropped by the server: an edit made
/// before that, on its entry, is refused as another editor's would be.
#[tokio::test]
async fn entries_dropped_by_the_server_conflict_with_an_older_edit() {
    let store = store("playlists-dropped").await;
    let list = playlist(&store, ALICE, vec![]).await;
    let r1 = add(&store, &list.id, 0, 1).await.unwrap();
    let r2 = add(&store, &list.id, r1, 2).await.unwrap();
    let gone = store.playlist_entries(&list.id).await.unwrap()[0].entry_id;
    let r3 = store
        .drop_playlist_entries(&list.id, &[gone])
        .await
        .unwrap();
    assert_eq!(r3, Some(r2 + 1));
    assert_eq!(order(&store, &list.id).await, ["plex:episode:2"]);
    let late = store
        .edit_playlist(&list.id, r2, vec![EntryEdit::Remove(gone)])
        .await;
    assert!(matches!(late, Err(PlaylistError::Conflict)));
    // Dropping what is not there changes nothing.
    assert_eq!(
        store
            .drop_playlist_entries(&list.id, &[gone])
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn a_relinked_entry_keeps_its_place_and_id() {
    let store = store("playlists-relink").await;
    let list = playlist(&store, ALICE, vec![]).await;
    let r1 = add(&store, &list.id, 0, 1).await.unwrap();
    add(&store, &list.id, r1, 2).await.unwrap();
    let entry = store.playlist_entries(&list.id).await.unwrap()[0].clone();
    assert_eq!(
        store.playlist_entry_ids(entry.entry_id).await.unwrap(),
        ["tvdb://1"]
    );
    store
        .relink_playlist_entry(&list.id, entry.entry_id, "plex:episode:901")
        .await
        .unwrap();
    let entries = store.playlist_entries(&list.id).await.unwrap();
    assert_eq!(entries[0].entry_id, entry.entry_id);
    assert_eq!(entries[0].media_id, "plex:episode:901");
}

#[tokio::test]
async fn pins_keep_the_order_the_user_sets() {
    let store = store("playlists-pins").await;
    let a = playlist(&store, ALICE, vec![]).await;
    let b = playlist(&store, ALICE, vec![]).await;
    let c = playlist(&store, ALICE, vec![]).await;
    store.pin_playlist(ALICE, &a.id, 0).await.unwrap();
    store.pin_playlist(ALICE, &b.id, 9).await.unwrap();
    store.pin_playlist(ALICE, &c.id, 0).await.unwrap();
    assert_eq!(
        store.pins(ALICE).await.unwrap(),
        [&c.id, &a.id, &b.id].map(String::clone)
    );
    // Dragged: pinned again at another place.
    store.pin_playlist(ALICE, &c.id, 2).await.unwrap();
    assert_eq!(
        store.pins(ALICE).await.unwrap(),
        [&a.id, &b.id, &c.id].map(String::clone)
    );
    assert!(store.unpin_playlist(ALICE, &b.id).await.unwrap());
    assert!(!store.unpin_playlist(ALICE, &b.id).await.unwrap());
    assert_eq!(
        store.pins(ALICE).await.unwrap(),
        [&a.id, &c.id].map(String::clone)
    );
    assert!(store.pins(BOB).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_pinned_playlist_unshared_from_a_user_leaves_pinned_with_a_note() {
    let store = store("playlists-unshared").await;
    let shares = vec![
        ShareRow {
            principal: BOB.into(),
            can_edit: false,
        },
        ShareRow {
            principal: CAROL.into(),
            can_edit: true,
        },
    ];
    let list = playlist(&store, ALICE, shares).await;
    for who in [ALICE, BOB, CAROL] {
        store.pin_playlist(who, &list.id, 0).await.unwrap();
    }
    store
        .update_playlist(
            &list.id,
            &PlaylistChange {
                shares: Some(vec![ShareRow {
                    principal: CAROL.into(),
                    can_edit: true,
                }]),
                ..PlaylistChange::default()
            },
        )
        .await
        .unwrap();
    assert!(store.pins(BOB).await.unwrap().is_empty());
    let notes = store.pin_notes(BOB).await.unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(
        (notes[0].playlist_id.as_str(), notes[0].reason),
        (list.id.as_str(), PinNoteReason::Unshared)
    );
    // The owner and a user it is still shared with keep theirs.
    assert_eq!(
        store.pins(ALICE).await.unwrap(),
        [&list.id].map(String::clone)
    );
    assert_eq!(
        store.pins(CAROL).await.unwrap(),
        [&list.id].map(String::clone)
    );
    assert!(store.pin_notes(CAROL).await.unwrap().is_empty());
    // Kept until a client dismisses it.
    assert_eq!(store.pin_notes(BOB).await.unwrap().len(), 1);
    assert!(store.dismiss_pin_notes(BOB).await.unwrap());
    assert!(store.pin_notes(BOB).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_server_wide_playlist_made_personal_leaves_everyone_else_s_pinned() {
    let store = store("playlists-unshared-server-wide").await;
    let list = store
        .create_playlist(&NewPlaylist {
            owner: ALICE.into(),
            name: "Specials".into(),
            kinds: EPISODES,
            server_wide: true,
            shares: vec![],
        })
        .await
        .unwrap();
    store.pin_playlist(BOB, &list.id, 0).await.unwrap();
    store
        .update_playlist(
            &list.id,
            &PlaylistChange {
                server_wide: Some(false),
                ..PlaylistChange::default()
            },
        )
        .await
        .unwrap();
    assert!(store.pins(BOB).await.unwrap().is_empty());
    assert_eq!(store.pin_notes(BOB).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_deleted_playlist_leaves_pinned_with_a_note_for_all_but_who_deleted_it() {
    let store = store("playlists-deleted").await;
    let list = playlist(
        &store,
        ALICE,
        vec![ShareRow {
            principal: BOB.into(),
            can_edit: false,
        }],
    )
    .await;
    store.pin_playlist(ALICE, &list.id, 0).await.unwrap();
    store.pin_playlist(BOB, &list.id, 0).await.unwrap();
    add(&store, &list.id, 0, 1).await.unwrap();
    store.delete_playlist(&list.id, ALICE).await.unwrap();
    assert!(store.playlist(&list.id).await.unwrap().is_none());
    assert!(store.playlist_entries(&list.id).await.unwrap().is_empty());
    assert!(store.pins(BOB).await.unwrap().is_empty());
    let notes = store.pin_notes(BOB).await.unwrap();
    assert_eq!(
        (notes[0].name.as_str(), notes[0].reason),
        ("Heists", PinNoteReason::Deleted)
    );
    assert!(store.pin_notes(ALICE).await.unwrap().is_empty());
    assert!(matches!(
        store.delete_playlist(&list.id, ALICE).await,
        Err(PlaylistError::NotFound)
    ));
}

#[tokio::test]
async fn kinds_and_names_change_with_the_revision() {
    let store = store("playlists-update").await;
    let list = playlist(&store, ALICE, vec![]).await;
    let more = Kinds {
        movies: true,
        ..EPISODES
    };
    let revision = store
        .update_playlist(
            &list.id,
            &PlaylistChange {
                name: Some("Capers".into()),
                kinds: Some(more),
                ..PlaylistChange::default()
            },
        )
        .await
        .unwrap();
    let after = store.playlist(&list.id).await.unwrap().unwrap();
    assert_eq!(
        (after.name.as_str(), after.kinds, after.revision),
        ("Capers", more, revision)
    );
}

/// A portable user who leaves the server (PX-47) takes their playlists
/// with them, and every pin, share and place of theirs.
#[tokio::test]
async fn a_portable_user_who_leaves_takes_their_playlists() {
    use spoolframe_protocol::PortableUserId;
    let store = store("playlists-leave").await;
    let portable = PortableUserId::from_bytes([7; 32]).unwrap();
    let guest = crate::principal::Principal::PortableUser(portable).to_string();
    let theirs = playlist(&store, &guest, vec![]).await;
    let alices = playlist(
        &store,
        ALICE,
        vec![ShareRow {
            principal: guest.clone(),
            can_edit: true,
        }],
    )
    .await;
    store.pin_playlist(ALICE, &theirs.id, 0).await.unwrap();
    store.pin_playlist(&guest, &alices.id, 0).await.unwrap();
    store.leave_portable(portable).await.unwrap();
    assert!(store.playlist(&theirs.id).await.unwrap().is_none());
    assert!(
        store
            .playlist(&alices.id)
            .await
            .unwrap()
            .unwrap()
            .shares
            .is_empty()
    );
    assert!(store.pins(&guest).await.unwrap().is_empty());
    // Who had pinned theirs is told.
    assert_eq!(store.pin_notes(ALICE).await.unwrap().len(), 1);
}

#[tokio::test]
async fn an_item_s_playlists_are_those_its_viewer_sees_that_hold_it() {
    let store = store("playlists-holding").await;
    let alices = playlist(&store, ALICE, vec![]).await;
    let shared = playlist(
        &store,
        ALICE,
        vec![ShareRow {
            principal: BOB.into(),
            can_edit: false,
        }],
    )
    .await;
    let empty = playlist(&store, ALICE, vec![]).await;
    add(&store, &alices.id, 0, 1).await.unwrap();
    add(&store, &shared.id, 0, 2).await.unwrap();
    let show = ["plex:show:1".to_owned()];
    let alice = store
        .playlists_holding(ALICE, HeldBy::Show, &show)
        .await
        .unwrap();
    assert_eq!(ids(&alice), [&alices.id, &shared.id]);
    let bob = store
        .playlists_holding(BOB, HeldBy::Show, &show)
        .await
        .unwrap();
    assert_eq!(ids(&bob), [&shared.id]);
    let one = store
        .playlists_holding(ALICE, HeldBy::Item, &["plex:episode:2".to_owned()])
        .await
        .unwrap();
    assert_eq!(ids(&one), [&shared.id]);
    assert!(!ids(&alice).contains(&empty.id.as_str()));
    assert!(
        store
            .playlists_holding(CAROL, HeldBy::Show, &show)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_place_and_its_settings_are_kept_per_user_and_playlist() {
    let store = store("playlists-place").await;
    let list = playlist(&store, ALICE, vec![]).await;
    assert_eq!(store.playlist_place(ALICE, &list.id).await.unwrap(), None);
    let place = PlaceRow {
        entry: Some(7),
        settings: PlaySettings {
            shuffle: true,
            repeat: Repeat::All,
            skip_watched: true,
        },
        round: Some(vec![9, 7, 8]),
    };
    store
        .set_playlist_place(ALICE, &list.id, &place)
        .await
        .unwrap();
    assert_eq!(
        store.playlist_place(ALICE, &list.id).await.unwrap(),
        Some(place.clone())
    );
    assert_eq!(store.playlist_place(BOB, &list.id).await.unwrap(), None);
    // The end clears the place, and keeps the settings.
    let ended = PlaceRow {
        entry: None,
        round: None,
        ..place
    };
    store
        .set_playlist_place(ALICE, &list.id, &ended)
        .await
        .unwrap();
    assert_eq!(
        store.playlist_place(ALICE, &list.id).await.unwrap(),
        Some(ended)
    );
}

/// PX-43: each user has one Liked and one All, however often they are
/// asked for, and only Liked's entries count as liked.
#[tokio::test]
async fn a_user_s_builtin_playlists_are_made_once() {
    let store = store("playlists-builtin").await;
    let liked = store.builtin_playlist(ALICE, Builtin::Liked).await.unwrap();
    assert_eq!(liked.builtin, Some(Builtin::Liked));
    assert_eq!(liked.name, "Liked");
    assert!(liked.kinds.tracks && !liked.kinds.movies && !liked.kinds.episodes);
    let again = store.builtin_playlist(ALICE, Builtin::Liked).await.unwrap();
    assert_eq!(again.id, liked.id);
    let all = store.builtin_playlist(ALICE, Builtin::All).await.unwrap();
    assert_ne!(all.id, liked.id);
    let bobs = store.builtin_playlist(BOB, Builtin::Liked).await.unwrap();
    assert_ne!(bobs.id, liked.id);

    let mut track = episode(1);
    track.media_id = "plex:track:7".into();
    store
        .edit_playlist(
            &liked.id,
            liked.revision,
            vec![EntryEdit::Add {
                entries: vec![track],
                before: None,
            }],
        )
        .await
        .unwrap();
    let other = playlist(&store, ALICE, Vec::new()).await;
    store
        .edit_playlist(
            &other.id,
            other.revision,
            vec![EntryEdit::Add {
                entries: vec![episode(2)],
                before: None,
            }],
        )
        .await
        .unwrap();
    let liked_now = store.liked_media(ALICE).await.unwrap();
    assert_eq!(liked_now.into_iter().collect::<Vec<_>>(), ["plex:track:7"]);
    assert!(store.liked_media(BOB).await.unwrap().is_empty());
    assert_eq!(
        store.playlist(&all.id).await.unwrap().unwrap().builtin,
        Some(Builtin::All)
    );
}
