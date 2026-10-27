//! Playlists through [`Playlists`] itself: a store, the fake catalog and
//! the stand-in ffprobe, no connection. `tests/it/playlists.rs` keeps the
//! requests' wiring.

use std::path::PathBuf;
use std::sync::Arc;

use spoolframe_media::Prober;
use spoolframe_media::sandbox::Tool;
use spoolframe_protocol::playlists::{
    Kinds, PinNoteReason, PlaySettings, PlaylistEdit, PlaylistNext, PlaylistOwnerKind,
    PlaylistShare, Repeat, Visibility,
};
use spoolframe_types::{Item, MediaId, MediaKind, Namespace, Role};

use super::*;
use crate::principal::{Actor, Principal};
use crate::test_support::{FakeCatalog, OldKey, fake_ffprobe, movie, write_episode};
use crate::watch::{Watch, WatchRules};

const EPISODES: Kinds = Kinds {
    movies: false,
    episodes: true,
    tracks: false,
};
const MUSIC: Kinds = Kinds {
    movies: false,
    episodes: false,
    tracks: true,
};

struct Harness {
    // Declared first, so the store closes before its directory goes.
    playlists: Playlists,
    catalog: Arc<FakeCatalog>,
    media: PathBuf,
    alice: Actor,
    bob: Actor,
    carol: Actor,
    admin: Actor,
    _dir: spoolframe_test_dir::TestDir,
}

async fn harness(test: &str) -> Harness {
    let dir = crate::store::tests::state_dir(&format!("playlists-{test}")).await;
    let store = Store::open(dir.as_ref()).await.unwrap();
    let media = dir.join("media").join("movie.mkv");
    let catalog = Arc::new(FakeCatalog::new(media.clone()));
    let ffprobe = fake_ffprobe(dir.as_ref(), "mkv-forced-subs.json");
    let prober = Arc::new(Prober::new(
        Tool::unconfined(ffprobe),
        Prober::DEFAULT_TIMEOUT,
    ));
    let watch = Arc::new(Watch::new(
        store.clone(),
        catalog.clone(),
        prober,
        WatchRules::default(),
    ));
    let mut actors = Vec::new();
    for (name, role) in [
        ("alice", Role::Consumer),
        ("bob", Role::Consumer),
        ("carol", Role::Consumer),
        ("root", Role::Admin),
    ] {
        let id = store.add_user(name, None, "phc", role).await.unwrap();
        actors.push(Actor {
            principal: Principal::LocalUser(id),
            admin: role == Role::Admin,
            federated: false,
        });
    }
    // Named seeds, one after another: each test's rounds are the same at
    // every run.
    let next_seed = std::sync::atomic::AtomicU64::new(1);
    let playlists = Playlists::new(store, catalog.clone(), watch)
        .with_seeds(move || next_seed.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    Harness {
        playlists,
        catalog,
        media,
        alice: actors[0],
        bob: actors[1],
        carol: actors[2],
        admin: actors[3],
        _dir: dir,
    }
}

fn id(kind: MediaKind, key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, kind, key).unwrap()
}

fn episode(key: &str) -> MediaId {
    id(MediaKind::Episode, key)
}

fn add(item: MediaId) -> PlaylistEdit {
    PlaylistEdit::Add {
        id: item,
        before: None,
    }
}

fn share(actor: &Actor, can_edit: bool) -> PlaylistShare {
    PlaylistShare::new(actor.principal.to_string(), can_edit)
}

impl Harness {
    /// The files of the movie and of `episodes`, readable.
    fn files(&self, episodes: &[&str]) {
        std::fs::create_dir_all(self.media.parent().unwrap()).unwrap();
        std::fs::write(&self.media, b"stand-in").unwrap();
        for key in episodes {
            write_episode(&self.media, key);
        }
    }

    async fn make(&self, owner: &Actor, kinds: Kinds, visibility: Visibility) -> String {
        self.playlists
            .create(owner, "Heists", kinds, visibility)
            .await
            .unwrap()
            .id
    }

    async fn edit(&self, who: &Actor, playlist: &str, edits: Vec<PlaylistEdit>) -> u64 {
        let revision = self.revision(who, playlist).await;
        self.playlists
            .edit(who, playlist, revision, edits)
            .await
            .unwrap()
    }

    async fn revision(&self, who: &Actor, playlist: &str) -> u64 {
        self.playlists
            .get(who, playlist, 0, 0)
            .await
            .unwrap()
            .playlist
            .revision
    }

    /// The items `who` sees in `playlist`, in order.
    async fn items(&self, who: &Actor, playlist: &str) -> Vec<String> {
        self.playlists
            .get(who, playlist, 0, MAX_PAGE)
            .await
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.item.id().to_string())
            .collect()
    }
}

#[tokio::test]
async fn an_episodes_playlist_refuses_a_film_until_movies_are_ticked() {
    let h = harness("kinds").await;
    let list = h.make(&h.alice, EPISODES, Visibility::Personal).await;
    let refused = h
        .playlists
        .edit(&h.alice, &list, 0, vec![add(movie().id().clone())])
        .await;
    assert!(
        matches!(
            refused,
            Err(PlaylistsError::KindNotAllowed(MediaKind::Movie))
        ),
        "{refused:?}"
    );
    h.playlists
        .update(
            &h.alice,
            &list,
            PlaylistUpdate {
                kinds: Some(Kinds {
                    movies: true,
                    ..EPISODES
                }),
                ..PlaylistUpdate::default()
            },
        )
        .await
        .unwrap();
    h.edit(&h.alice, &list, vec![add(movie().id().clone())])
        .await;
    assert_eq!(h.items(&h.alice, &list).await, ["plex:movie:1"]);
}

#[tokio::test]
async fn a_music_playlist_refuses_a_film_sent_anyway() {
    let h = harness("music-only").await;
    let list = h.make(&h.alice, MUSIC, Visibility::Personal).await;
    let refused = h
        .playlists
        .edit(&h.alice, &list, 0, vec![add(movie().id().clone())])
        .await;
    assert!(matches!(
        refused,
        Err(PlaylistsError::KindNotAllowed(MediaKind::Movie))
    ));
}

#[tokio::test]
async fn kinds_are_never_narrowed() {
    let h = harness("narrow").await;
    let list = h
        .make(
            &h.alice,
            Kinds {
                movies: true,
                ..EPISODES
            },
            Visibility::Personal,
        )
        .await;
    let narrowed = h
        .playlists
        .update(
            &h.alice,
            &list,
            PlaylistUpdate {
                kinds: Some(EPISODES),
                ..PlaylistUpdate::default()
            },
        )
        .await;
    assert!(matches!(narrowed, Err(PlaylistsError::InvalidInput(_))));
}

/// A season adds its episodes, a show every episode with the specials
/// last, an album its tracks by number, an artist its albums' oldest
/// first.
#[tokio::test]
async fn a_season_show_album_or_artist_adds_its_items_in_order() {
    let h = harness("expand").await;
    let shows = h.make(&h.alice, EPISODES, Visibility::Personal).await;
    h.edit(&h.alice, &shows, vec![add(id(MediaKind::Season, "11"))])
        .await;
    h.edit(&h.alice, &shows, vec![add(id(MediaKind::Show, "10"))])
        .await;
    assert_eq!(
        h.items(&h.alice, &shows).await,
        [
            "plex:episode:12",
            "plex:episode:13",
            "plex:episode:12",
            "plex:episode:13",
            "plex:episode:15",
            "plex:episode:21",
        ]
    );
    let music = h.make(&h.alice, MUSIC, Visibility::Personal).await;
    h.edit(&h.alice, &music, vec![add(id(MediaKind::Album, "31"))])
        .await;
    h.edit(&h.alice, &music, vec![add(id(MediaKind::Artist, "30"))])
        .await;
    assert_eq!(
        h.items(&h.alice, &music).await,
        [
            "plex:track:32",
            "plex:track:33",
            "plex:track:35",
            "plex:track:32",
            "plex:track:33",
        ]
    );
}

/// The ticket's Unavailable: an item whose file cannot be read stays,
/// marked, and the header counts it; one removed from the library leaves.
#[tokio::test]
async fn an_unreadable_item_stays_unavailable_and_a_removed_one_leaves() {
    let h = harness("unavailable").await;
    h.files(&["12", "13", "15"]);
    let list = h.make(&h.alice, EPISODES, Visibility::Personal).await;
    h.edit(&h.alice, &list, vec![add(id(MediaKind::Season, "11"))])
        .await;
    h.edit(&h.alice, &list, vec![add(episode("15"))]).await;
    let before = h.revision(&h.alice, &list).await;
    h.catalog.unreadable(&episode("13"));
    h.catalog.remove(&episode("15"));
    let page = h.playlists.get(&h.alice, &list, 0, MAX_PAGE).await.unwrap();
    let shown: Vec<_> = page
        .entries
        .iter()
        .map(|e| (e.item.id().key().to_owned(), e.unavailable))
        .collect();
    assert_eq!(shown, [("12".into(), false), ("13".into(), true)]);
    assert_eq!((page.total, page.playable), (2, 1));
    // The drop is a change, which other editors and clients see.
    assert_eq!(page.playlist.revision, before + 1);
}

#[tokio::test]
async fn an_entry_follows_its_item_to_a_new_key() {
    let h = harness("relink").await;
    let list = h.make(&h.alice, EPISODES, Visibility::Personal).await;
    h.edit(
        &h.alice,
        &list,
        vec![add(episode("12")), add(episode("13"))],
    )
    .await;
    let entries = h
        .playlists
        .get(&h.alice, &list, 0, 10)
        .await
        .unwrap()
        .entries;
    let new = h.catalog.rekey(&episode("12"), "912");
    let after = h
        .playlists
        .get(&h.alice, &list, 0, 10)
        .await
        .unwrap()
        .entries;
    assert_eq!(after[0].item.id(), &new);
    // The same entry, in the same place.
    assert_eq!(after[0].entry, entries[0].entry);
    assert_eq!(after[1].entry, entries[1].entry);
}

#[tokio::test]
async fn a_catalog_that_cannot_be_asked_drops_nothing() {
    let h = harness("unreachable").await;
    let list = h
        .make(
            &h.alice,
            Kinds {
                movies: true,
                ..EPISODES
            },
            Visibility::Personal,
        )
        .await;
    h.edit(&h.alice, &list, vec![add(movie().id().clone())])
        .await;
    h.catalog.set_old_key(movie().id(), OldKey::Unreachable);
    let failed = h.playlists.get(&h.alice, &list, 0, 10).await;
    assert!(matches!(failed, Err(PlaylistsError::Catalog(_))));
    h.catalog.forget_old_key(movie().id());
    assert_eq!(h.items(&h.alice, &list).await, ["plex:movie:1"]);
}

#[tokio::test]
async fn sharing_gives_read_or_edit_and_a_third_user_sees_nothing() {
    let h = harness("sharing").await;
    let list = h
        .make(
            &h.alice,
            EPISODES,
            Visibility::Shared {
                with: vec![share(&h.bob, false), share(&h.carol, true)],
            },
        )
        .await;
    let revision = h.revision(&h.alice, &list).await;
    let read_only = h
        .playlists
        .edit(&h.bob, &list, revision, vec![add(episode("12"))])
        .await;
    assert!(matches!(read_only, Err(PlaylistsError::ReadOnly)));
    h.edit(&h.carol, &list, vec![add(episode("12"))]).await;
    assert_eq!(h.items(&h.bob, &list).await, ["plex:episode:12"]);
    // An editor changes entries only.
    let renamed = h
        .playlists
        .update(
            &h.carol,
            &list,
            PlaylistUpdate {
                name: Some("Mine now".into()),
                ..PlaylistUpdate::default()
            },
        )
        .await;
    assert!(matches!(renamed, Err(PlaylistsError::ReadOnly)));
    assert!(matches!(
        h.playlists.delete(&h.carol, &list).await,
        Err(PlaylistsError::ReadOnly)
    ));
    // Not shared with the admin: an admin sees no one's personal list.
    assert!(matches!(
        h.playlists.get(&h.admin, &list, 0, 10).await,
        Err(PlaylistsError::NotFound)
    ));
    let (bobs, _) = h.playlists.list(&h.bob).await.unwrap();
    assert_eq!(bobs[0].owner_kind, PlaylistOwnerKind::SharedWithMe);
    // Who else it is shared with is not Bob's to know.
    assert_eq!(bobs[0].visibility, Visibility::Shared { with: vec![] });
    let (alices, _) = h.playlists.list(&h.alice).await.unwrap();
    assert_eq!(alices[0].owner_kind, PlaylistOwnerKind::Mine);
    assert!(matches!(&alices[0].visibility, Visibility::Shared { with } if with.len() == 2));
}

#[tokio::test]
async fn a_server_wide_playlist_is_made_by_an_admin_seen_by_all_and_edited_by_admins() {
    let h = harness("server-wide").await;
    let refused = h
        .playlists
        .create(&h.alice, "Ours", EPISODES, Visibility::ServerWide)
        .await;
    assert!(matches!(refused, Err(PlaylistsError::AdminRequired)));
    let list = h.make(&h.admin, EPISODES, Visibility::ServerWide).await;
    let guest = Actor {
        principal: Principal::PortableUser(
            spoolframe_protocol::PortableUserId::from_bytes([3; 32]).unwrap(),
        ),
        admin: false,
        federated: false,
    };
    for who in [&h.alice, &h.bob, &guest] {
        let (seen, _) = h.playlists.list(who).await.unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].owner_kind, PlaylistOwnerKind::ServerWide);
        assert!(!seen[0].can_edit);
    }
    let other_admin = Actor {
        principal: h.carol.principal,
        admin: true,
        federated: false,
    };
    h.edit(&other_admin, &list, vec![add(episode("12"))]).await;
    assert_eq!(h.items(&h.alice, &list).await, ["plex:episode:12"]);
}

#[tokio::test]
async fn sharing_with_someone_who_is_not_a_user_is_refused() {
    let h = harness("share-stranger").await;
    let stranger = PlaylistShare::new("local:0192f1c4-5b6e-7a3d-9c41-2e8f7b6a5d4c", false);
    let refused = h
        .playlists
        .create(
            &h.alice,
            "Heists",
            EPISODES,
            Visibility::Shared {
                with: vec![stranger],
            },
        )
        .await;
    assert!(matches!(refused, Err(PlaylistsError::InvalidInput(_))));
}

/// The ticket's item screens: the series screen shows the heist playlist
/// for its owner and a user it is shared with, not for a third; a
/// server-wide one for everyone. The same for a film and an album.
#[tokio::test]
async fn an_item_s_screen_lists_the_playlists_its_viewer_sees_holding_it() {
    let h = harness("holding").await;
    let heists = h
        .make(
            &h.alice,
            Kinds {
                movies: true,
                episodes: true,
                tracks: true,
            },
            Visibility::Shared {
                with: vec![share(&h.bob, false)],
            },
        )
        .await;
    h.edit(
        &h.alice,
        &heists,
        vec![
            add(episode("13")),
            add(movie().id().clone()),
            add(id(MediaKind::Track, "35")),
        ],
    )
    .await;
    let everyone = h.make(&h.admin, EPISODES, Visibility::ServerWide).await;
    h.edit(&h.admin, &everyone, vec![add(episode("15"))]).await;

    let show = crate::test_support::show();
    let album = crate::test_support::album("34");
    let artist = crate::test_support::artist();
    for screen in [&movie(), &album, &artist] {
        let names = |refs: Vec<spoolframe_protocol::playlists::PlaylistRef>| {
            refs.into_iter().map(|r| r.id).collect::<Vec<_>>()
        };
        assert_eq!(
            names(h.playlists.holding(&h.alice, screen, &[]).await.unwrap()),
            std::slice::from_ref(&heists),
            "{screen:?}"
        );
        assert_eq!(
            names(h.playlists.holding(&h.bob, screen, &[]).await.unwrap()),
            std::slice::from_ref(&heists)
        );
        assert!(
            h.playlists
                .holding(&h.carol, screen, &[])
                .await
                .unwrap()
                .is_empty()
        );
    }
    let for_carol = h.playlists.holding(&h.carol, &show, &[]).await.unwrap();
    assert_eq!(for_carol.len(), 1);
    assert_eq!(
        (for_carol[0].id.as_str(), for_carol[0].owner_kind),
        (everyone.as_str(), PlaylistOwnerKind::ServerWide)
    );
    assert_eq!(
        h.playlists.holding(&h.bob, &show, &[]).await.unwrap().len(),
        2
    );
}

/// Hides one episode from Bob, as a later per-user rule might.
#[derive(Debug)]
struct NotForBob(Principal);

impl ItemVisibility for NotForBob {
    fn may_see(&self, actor: &Actor, item: &Item) -> bool {
        actor.principal != self.0 || item.id().key() != "13"
    }
}

#[tokio::test]
async fn an_item_a_user_may_not_see_is_neither_shown_nor_counted() {
    let h = harness("hidden").await;
    // The rule names this harness's Bob, known once he is made.
    let h = Harness {
        playlists: Playlists::with_visibility(
            h.playlists.store.clone(),
            h.catalog.clone(),
            h.playlists.watch.clone(),
            Arc::new(NotForBob(h.bob.principal)),
        ),
        ..h
    };
    h.files(&["12", "13"]);
    let list = h
        .make(
            &h.alice,
            EPISODES,
            Visibility::Shared {
                with: vec![share(&h.bob, false)],
            },
        )
        .await;
    h.edit(&h.alice, &list, vec![add(id(MediaKind::Season, "11"))])
        .await;
    let bobs = h.playlists.get(&h.bob, &list, 0, 10).await.unwrap();
    assert_eq!(bobs.total, 1);
    assert_eq!(bobs.entries[0].item.id(), &episode("12"));
    assert_eq!(
        h.playlists.get(&h.alice, &list, 0, 10).await.unwrap().total,
        2
    );
}

#[tokio::test]
async fn pages_are_cut_from_the_whole_list() {
    let h = harness("pages").await;
    let list = h.make(&h.alice, EPISODES, Visibility::Personal).await;
    h.edit(&h.alice, &list, vec![add(id(MediaKind::Show, "10"))])
        .await;
    let page = h.playlists.get(&h.alice, &list, 1, 2).await.unwrap();
    let keys: Vec<_> = page.entries.iter().map(|e| e.item.id().key()).collect();
    assert_eq!(keys, ["13", "15"]);
    assert_eq!((page.offset, page.total), (1, 4));
}

#[tokio::test]
async fn unsharing_a_pinned_playlist_tells_who_lost_it_and_leaves_a_note() {
    let h = harness("unshare").await;
    let list = h
        .make(
            &h.alice,
            EPISODES,
            Visibility::Shared {
                with: vec![share(&h.bob, false)],
            },
        )
        .await;
    h.playlists.pin(&h.bob, &list, 0).await.unwrap();
    let mut news = h.playlists.news();
    h.playlists
        .update(
            &h.alice,
            &list,
            PlaylistUpdate {
                visibility: Some(Visibility::Personal),
                ..PlaylistUpdate::default()
            },
        )
        .await
        .unwrap();
    match news.recv().await.unwrap() {
        PlaylistNews::Playlist { before, after, .. } => {
            assert!(before.unwrap().sees(h.bob.principal));
            assert!(!after.unwrap().sees(h.bob.principal));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        news.recv().await.unwrap(),
        PlaylistNews::Pins {
            principal: h.bob.principal
        }
    );
    let (lists, notes) = h.playlists.list(&h.bob).await.unwrap();
    assert!(lists.is_empty());
    assert_eq!(notes[0].reason, PinNoteReason::Unshared);
    h.playlists.dismiss_pin_notes(&h.bob).await.unwrap();
    assert!(h.playlists.list(&h.bob).await.unwrap().1.is_empty());
}

#[tokio::test]
async fn a_playlist_one_does_not_see_cannot_be_pinned() {
    let h = harness("pin-unseen").await;
    let list = h.make(&h.alice, EPISODES, Visibility::Personal).await;
    assert!(matches!(
        h.playlists.pin(&h.bob, &list, 0).await,
        Err(PlaylistsError::NotFound)
    ));
    h.playlists.pin(&h.alice, &list, 0).await.unwrap();
    let (lists, _) = h.playlists.list(&h.alice).await.unwrap();
    assert_eq!(lists[0].pinned, Some(0));
}

/// A playlist of episodes from several seasons, out of the show's order,
/// with every file readable: 15 (S2E1, which has credits), 12, 21.
async fn heists(h: &Harness) -> String {
    h.files(&["12", "13", "15", "21"]);
    let list = h.make(&h.alice, EPISODES, Visibility::Personal).await;
    h.edit(
        &h.alice,
        &list,
        vec![add(episode("15")), add(episode("12")), add(episode("21"))],
    )
    .await;
    list
}

fn settings() -> PlaySettings {
    PlaySettings::default()
}

fn key(next: &Option<PlaylistNext>) -> &str {
    next.as_ref().expect("something to play").item.id().key()
}

/// The ticket's B99 heists: in the playlist's order across seasons, each
/// with its credits to skip; the end clears the place, so the next play
/// starts at the top.
#[tokio::test]
async fn a_playlist_plays_in_its_order_with_credits_and_starts_over_after_the_end() {
    let h = harness("play-order").await;
    let list = heists(&h).await;
    let first = h
        .playlists
        .play(&h.alice, &list, None, settings())
        .await
        .unwrap();
    assert_eq!(key(&first), "15");
    let credits = first.as_ref().unwrap().markers.iter();
    assert!(
        credits
            .clone()
            .any(|m| m.kind == spoolframe_types::MarkerKind::Credits)
    );
    let mut after = first.unwrap().entry;
    let mut played = Vec::new();
    while let Some(next) = h.playlists.next(&h.alice, &list, after).await.unwrap() {
        played.push(next.item.id().key().to_owned());
        after = next.entry;
    }
    assert_eq!(played, ["12", "21"]);
    let page = h.playlists.get(&h.alice, &list, 0, 10).await.unwrap();
    assert_eq!(page.place, None);
    let again = h
        .playlists
        .play(&h.alice, &list, None, settings())
        .await
        .unwrap();
    assert_eq!(key(&again), "15");
}

/// The next day, Continue picks up at the one after the last finished;
/// one left half-watched is picked up itself.
#[tokio::test]
async fn continuing_resumes_after_the_finished_one_or_at_the_one_in_progress() {
    let h = harness("play-resume").await;
    let list = heists(&h).await;
    let first = h
        .playlists
        .play(&h.alice, &list, None, settings())
        .await
        .unwrap();
    h.playlists
        .watch
        .mark(h.alice.principal, &episode("15"), true)
        .await
        .unwrap();
    let next_day = h
        .playlists
        .play(&h.alice, &list, None, settings())
        .await
        .unwrap();
    assert_eq!(key(&next_day), "12");
    assert_ne!(next_day.as_ref().unwrap().entry, first.unwrap().entry);
    h.playlists
        .watch
        .progress(
            h.alice.principal,
            &episode("12"),
            std::time::Duration::from_secs(600),
        )
        .await
        .unwrap();
    let later = h
        .playlists
        .play(&h.alice, &list, None, settings())
        .await
        .unwrap();
    assert_eq!(key(&later), "12");
    // With its position, to start from where they were.
    let Item::Episode(e) = &later.unwrap().item else {
        panic!("not an episode");
    };
    assert!(e.playback.position.is_some());
}

#[tokio::test]
async fn skip_watched_passes_a_watched_one() {
    let h = harness("play-skip-watched").await;
    let list = heists(&h).await;
    h.playlists
        .watch
        .mark(h.alice.principal, &episode("12"), true)
        .await
        .unwrap();
    let skipping = PlaySettings {
        skip_watched: true,
        ..settings()
    };
    let first = h
        .playlists
        .play(&h.alice, &list, None, skipping)
        .await
        .unwrap();
    assert_eq!(key(&first), "15");
    let next = h
        .playlists
        .next(&h.alice, &list, first.unwrap().entry)
        .await
        .unwrap();
    assert_eq!(key(&next), "21");
}

#[tokio::test]
async fn an_unavailable_entry_is_skipped() {
    let h = harness("play-unavailable").await;
    let list = heists(&h).await;
    h.catalog.unreadable(&episode("12"));
    let first = h
        .playlists
        .play(&h.alice, &list, None, settings())
        .await
        .unwrap();
    let next = h
        .playlists
        .next(&h.alice, &list, first.unwrap().entry)
        .await
        .unwrap();
    assert_eq!(key(&next), "21");
}

/// The ticket: shuffle with repeat gives every item once per round.
#[tokio::test]
async fn shuffle_and_repeat_give_every_entry_once_per_round() {
    let h = harness("play-shuffle").await;
    let list = heists(&h).await;
    h.edit(&h.alice, &list, vec![add(episode("13"))]).await;
    let shuffled = PlaySettings {
        shuffle: true,
        repeat: Repeat::All,
        skip_watched: false,
    };
    let mut next = h
        .playlists
        .play(&h.alice, &list, None, shuffled)
        .await
        .unwrap();
    let mut played = Vec::new();
    for _ in 0..8 {
        let now = next.expect("repeat all never ends");
        played.push(now.item.id().key().to_owned());
        next = h.playlists.next(&h.alice, &list, now.entry).await.unwrap();
    }
    for round in played.chunks(4) {
        let mut round = round.to_vec();
        round.sort();
        assert_eq!(round, ["12", "13", "15", "21"], "{played:?}");
    }
}

const SHUFFLED: PlaySettings = PlaySettings {
    shuffle: true,
    repeat: Repeat::Off,
    skip_watched: false,
};

impl Harness {
    /// The keys `who` plays in `list` from `first` on, by `next`, at most
    /// `count` of them.
    async fn playing_on(
        &self,
        who: &Actor,
        list: &str,
        first: Option<PlaylistNext>,
        count: usize,
    ) -> Vec<String> {
        let mut played = Vec::new();
        let mut now = first;
        while let Some(playing) = now {
            played.push(playing.item.id().key().to_owned());
            if played.len() == count {
                break;
            }
            now = self.playlists.next(who, list, playing.entry).await.unwrap();
        }
        played
    }

    /// The keys `who` plays in `list` from a play with `settings` on.
    async fn played(
        &self,
        who: &Actor,
        list: &str,
        settings: PlaySettings,
        count: usize,
    ) -> Vec<String> {
        let first = self
            .playlists
            .play(who, list, None, settings)
            .await
            .unwrap();
        self.playing_on(who, list, first, count).await
    }
}

/// The user's rule: one shuffle per turn-on, kept. Repeat All plays the
/// same round again, not a new one.
#[tokio::test]
async fn repeat_all_replays_the_same_round_while_shuffling() {
    let h = harness("play-shuffle-replay").await;
    let list = heists(&h).await;
    h.edit(&h.alice, &list, vec![add(episode("13"))]).await;
    let looping = PlaySettings {
        repeat: Repeat::All,
        ..SHUFFLED
    };
    let played = h.played(&h.alice, &list, looping, 8).await;
    assert_eq!(played[..4], played[4..], "{played:?}");
}

/// Listened through to the end and played again, the round is the same:
/// shuffle was not turned off and on.
#[tokio::test]
async fn playing_again_after_the_end_keeps_the_round() {
    let h = harness("play-shuffle-again").await;
    let list = heists(&h).await;
    h.edit(&h.alice, &list, vec![add(episode("13"))]).await;
    let first = h.played(&h.alice, &list, SHUFFLED, 10).await;
    let again = h.played(&h.alice, &list, SHUFFLED, 10).await;
    assert_eq!(again, first);
}

/// An entry added while shuffling goes somewhere among those not played
/// yet, not always last.
#[tokio::test]
async fn an_entry_added_while_shuffling_plays_before_the_round_ends() {
    let h = harness("play-shuffle-added").await;
    let list = heists(&h).await;
    let first = h
        .playlists
        .play(&h.alice, &list, None, SHUFFLED)
        .await
        .unwrap();
    h.edit(&h.alice, &list, vec![add(episode("13"))]).await;
    let played = h.playing_on(&h.alice, &list, first, 10).await;
    assert_ne!(played.last().map(String::as_str), Some("13"), "{played:?}");
}

/// Shuffle turned on part way through, with no entry picked, starts its
/// round at the entry playing: every other entry is ahead, and plays once.
#[tokio::test]
async fn turning_shuffle_on_without_a_pick_puts_the_whole_playlist_ahead() {
    let h = harness("play-shuffle-on").await;
    let list = heists(&h).await;
    h.edit(&h.alice, &list, vec![add(episode("13"))]).await;
    h.playlists
        .play(&h.alice, &list, None, settings())
        .await
        .unwrap();
    let mut played = h.played(&h.alice, &list, SHUFFLED, 10).await;
    played.sort();
    assert_eq!(played, ["12", "13", "15", "21"]);
}

impl Harness {
    /// The keys of what `who` has coming up in `list`.
    async fn upcoming(&self, who: &Actor, list: &str, limit: u32) -> Vec<String> {
        self.playlists
            .upcoming(who, list, limit)
            .await
            .unwrap()
            .iter()
            .map(|e| e.item.id().key().to_owned())
            .collect()
    }

    /// Play `list` with `settings`, give what plays, and four entries
    /// in all: the heists and 13.
    async fn playing_four(&self, settings: PlaySettings) -> (String, PlaylistNext) {
        let list = heists(self).await;
        self.edit(&self.alice, &list, vec![add(episode("13"))])
            .await;
        let first = self
            .playlists
            .play(&self.alice, &list, None, settings)
            .await
            .unwrap()
            .expect("something to play");
        (list, first)
    }
}

/// What comes up is what `next` plays, one after another: the round's
/// order while shuffling.
#[tokio::test]
async fn upcoming_is_what_next_plays_in_turn() {
    let h = harness("upcoming-in-turn").await;
    let (list, first) = h.playing_four(SHUFFLED).await;
    let upcoming = h.upcoming(&h.alice, &list, 10).await;
    let next = h
        .playlists
        .next(&h.alice, &list, first.entry)
        .await
        .unwrap();
    let played = h.playing_on(&h.alice, &list, next, 10).await;
    assert_eq!(upcoming, played);
}

/// With repeat all, it goes round to the round's start again.
#[tokio::test]
async fn upcoming_goes_round_with_repeat_all() {
    let h = harness("upcoming-repeat-all").await;
    let looping = PlaySettings {
        repeat: Repeat::All,
        ..SHUFFLED
    };
    let (list, first) = h.playing_four(looping).await;
    let upcoming = h.upcoming(&h.alice, &list, 7).await;
    let next = h
        .playlists
        .next(&h.alice, &list, first.entry)
        .await
        .unwrap();
    let played = h.playing_on(&h.alice, &list, next, 7).await;
    assert_eq!(upcoming, played);
}

/// With repeat one, what comes up is the entry playing, once.
#[tokio::test]
async fn upcoming_with_repeat_one_is_the_entry_playing() {
    let h = harness("upcoming-repeat-one").await;
    let one = PlaySettings {
        repeat: Repeat::One,
        ..settings()
    };
    let (list, first) = h.playing_four(one).await;
    let upcoming = h.playlists.upcoming(&h.alice, &list, 10).await.unwrap();
    let entries: Vec<EntryId> = upcoming.iter().map(|e| e.entry).collect();
    assert_eq!(entries, [first.entry]);
}

#[tokio::test]
async fn upcoming_without_a_place_is_the_playlist_from_the_top() {
    let h = harness("upcoming-no-place").await;
    let list = heists(&h).await;
    assert_eq!(h.upcoming(&h.alice, &list, 10).await, ["15", "12", "21"]);
}

#[tokio::test]
async fn upcoming_lists_at_most_its_limit() {
    let h = harness("upcoming-limit").await;
    let list = heists(&h).await;
    assert_eq!(h.upcoming(&h.alice, &list, 2).await, ["15", "12"]);
}

#[tokio::test]
async fn upcoming_of_a_playlist_one_does_not_see_is_not_found() {
    let h = harness("upcoming-unseen").await;
    let list = heists(&h).await;
    let refused = h.playlists.upcoming(&h.bob, &list, 10).await;
    assert!(
        matches!(refused, Err(PlaylistsError::NotFound)),
        "{refused:?}"
    );
}

/// Saving settings while shuffling plays nothing and keeps the round and
/// the place: what comes up is the same.
#[tokio::test]
async fn saving_settings_while_shuffling_keeps_the_round_and_the_place() {
    let h = harness("settings-keep").await;
    let (list, _) = h.playing_four(SHUFFLED).await;
    let before = h.upcoming(&h.alice, &list, 10).await;
    let skipping = PlaySettings {
        skip_watched: true,
        ..SHUFFLED
    };
    h.playlists
        .set_settings(&h.alice, &list, skipping)
        .await
        .unwrap();
    assert_eq!(h.upcoming(&h.alice, &list, 10).await, before);
}

/// A settings save that leaves shuffle on stores the round as it is,
/// without reading the entries (on All, the library): an entry added since
/// gets its place when the round is next played or listed.
#[tokio::test]
async fn saving_settings_with_shuffle_kept_leaves_the_round_as_it_is() {
    let h = harness("settings-round-as-is").await;
    let list = heists(&h).await;
    h.playlists
        .play(&h.alice, &list, None, SHUFFLED)
        .await
        .unwrap();
    let principal = h.alice.principal.to_string();
    let round = async || {
        h.playlists
            .store
            .playlist_place(&principal, &list)
            .await
            .unwrap()
            .expect("a place")
            .round
    };
    let before = round().await;
    h.edit(&h.alice, &list, vec![add(episode("13"))]).await;
    let looping = PlaySettings {
        repeat: Repeat::All,
        ..SHUFFLED
    };
    h.playlists
        .set_settings(&h.alice, &list, looping)
        .await
        .unwrap();
    assert_eq!(round().await, before);
}

/// Shuffle turned on by a settings save shuffles from the entry playing:
/// every other entry is ahead.
#[tokio::test]
async fn turning_shuffle_on_by_settings_puts_the_whole_playlist_ahead() {
    let h = harness("settings-shuffle-on").await;
    let (list, first) = h.playing_four(settings()).await;
    h.playlists
        .set_settings(&h.alice, &list, SHUFFLED)
        .await
        .unwrap();
    let mut upcoming = h.upcoming(&h.alice, &list, 10).await;
    upcoming.sort();
    let mut others = vec!["12", "13", "15", "21"];
    others.retain(|k| *k != first.item.id().key());
    assert_eq!(upcoming, others);
}

/// Shuffle turned off by a settings save goes on in the playlist's order
/// from the entry playing.
#[tokio::test]
async fn turning_shuffle_off_by_settings_goes_on_in_the_playlists_order() {
    let h = harness("settings-shuffle-off").await;
    let (list, first) = h.playing_four(SHUFFLED).await;
    h.playlists
        .set_settings(&h.alice, &list, settings())
        .await
        .unwrap();
    let order = ["15", "12", "21", "13"];
    let at = order
        .iter()
        .position(|k| *k == first.item.id().key())
        .unwrap();
    assert_eq!(h.upcoming(&h.alice, &list, 10).await, order[at + 1..]);
}

#[tokio::test]
async fn repeat_one_plays_the_same_entry_again_and_repeat_all_goes_round() {
    let h = harness("play-repeat").await;
    let list = heists(&h).await;
    let one = PlaySettings {
        repeat: Repeat::One,
        ..settings()
    };
    let first = h.playlists.play(&h.alice, &list, None, one).await.unwrap();
    let again = h
        .playlists
        .next(&h.alice, &list, first.as_ref().unwrap().entry)
        .await
        .unwrap();
    assert_eq!(again.unwrap().entry, first.unwrap().entry);
    let all = PlaySettings {
        repeat: Repeat::All,
        ..settings()
    };
    let page = h.playlists.get(&h.alice, &list, 0, 10).await.unwrap();
    let last = page.entries[2].entry;
    let from_last = h
        .playlists
        .play(&h.alice, &list, Some(last), all)
        .await
        .unwrap();
    assert_eq!(key(&from_last), "21");
    let round = h.playlists.next(&h.alice, &list, last).await.unwrap();
    assert_eq!(key(&round), "15");
}

/// A Plex import's mapping: the items found here make a personal playlist
/// of their kinds; what is not here is reported, and a dry run makes
/// nothing.
#[tokio::test]
async fn an_import_makes_a_playlist_of_what_is_here_and_reports_the_rest() {
    let h = harness("import").await;
    h.catalog.remove(&episode("13"));
    let items = vec![
        (episode("12"), "S1E1".to_owned()),
        (episode("13"), "S1E2".to_owned()),
        (id(MediaKind::Track, "32"), "Track 32".to_owned()),
    ];
    let (none, count, _) = h
        .playlists
        .import(&h.alice, "From Plex", &items, true)
        .await
        .unwrap();
    assert_eq!((none, count), (None, 2));
    assert!(h.playlists.list(&h.alice).await.unwrap().0.is_empty());
    let (made, count, left_out) = h
        .playlists
        .import(&h.alice, "From Plex", &items, false)
        .await
        .unwrap();
    assert_eq!(count, 2);
    assert_eq!(left_out.len(), 1);
    assert_eq!(left_out[0].0, "S1E2");
    let page = h
        .playlists
        .get(&h.alice, &made.unwrap(), 0, 10)
        .await
        .unwrap();
    assert_eq!(page.playlist.name, "From Plex");
    assert_eq!(
        page.playlist.kinds,
        Kinds {
            movies: false,
            episodes: true,
            tracks: true
        }
    );
    assert_eq!(page.playlist.owner_kind, PlaylistOwnerKind::Mine);
    assert_eq!(page.total, 2);
}
