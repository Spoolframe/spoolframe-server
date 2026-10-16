//! Liked and All (PX-43) through [`Playlists`] itself, against a music
//! library that can grow: an album gains a track, a scan brings a new
//! album, as a Plex scan would between two reads.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use spoolframe_catalog::{
    Catalog, CatalogError, Found, Image, ItemPage, ItemWithChildren, Page, SearchQuery,
};
use spoolframe_media::Prober;
use spoolframe_media::sandbox::Tool;
use spoolframe_protocol::playlists::{Builtin, Liked, PlaySettings, PlaylistEdit, Repeat};
use spoolframe_types::{
    Album, Artist, ArtworkId, Item, Library, LibraryId, LibraryKind, MediaId, MediaKind, Namespace,
    PersonId, PlaybackState, Role, Track,
};

use super::*;
use crate::principal::{Actor, Principal};
use crate::test_support::fake_ffprobe;
use crate::watch::{Watch, WatchRules};

fn id(kind: MediaKind, key: u32) -> MediaId {
    MediaId::new(Namespace::Plex, kind, key.to_string()).unwrap()
}

/// One artist, `plex:artist:1`, and its albums: album `n` is
/// `plex:album:<n*100>`, its tracks `plex:track:<n*100+1>` on.
#[derive(Debug)]
struct Music {
    /// Each album's key and how many tracks it has now.
    albums: Mutex<Vec<(u32, u32)>>,
}

impl Music {
    /// One album of 12 tracks, `plex:album:100`.
    fn new() -> Self {
        Self {
            albums: Mutex::new(vec![(100, 12)]),
        }
    }

    /// A scan found one more track on album `key`.
    fn add_track(&self, key: u32) {
        if let Some(album) = self.albums.lock().iter_mut().find(|(k, _)| *k == key) {
            album.1 += 1;
        }
    }

    /// A scan found a new album of `tracks` tracks: its key.
    fn add_album(&self, tracks: u32) -> u32 {
        let mut albums = self.albums.lock();
        let key = (albums.len() as u32 + 1) * 100;
        albums.push((key, tracks));
        key
    }

    fn library() -> Library {
        Library {
            id: LibraryId::new(Namespace::Plex, "9").unwrap(),
            title: "Music".into(),
            kind: LibraryKind::Music,
        }
    }

    fn artist() -> Item {
        Item::Artist(Artist {
            id: id(MediaKind::Artist, 1),
            name: "Daft Punk".into(),
            summary: None,
            artwork: Vec::new(),
        })
    }

    fn album(key: u32) -> Item {
        Item::Album(Album {
            id: id(MediaKind::Album, key),
            artist_id: id(MediaKind::Artist, 1),
            artist_name: Some("Daft Punk".into()),
            title: format!("Album {key}"),
            year: Some(2001),
            track_count: None,
            artwork: Vec::new(),
        })
    }

    fn track(album: u32, number: u32) -> Item {
        Item::Track(Track {
            id: id(MediaKind::Track, album + number),
            album_id: id(MediaKind::Album, album),
            artist_id: id(MediaKind::Artist, 1),
            artist_name: Some("Daft Punk".into()),
            album_title: Some(format!("Album {album}")),
            title: format!("Track {number}"),
            track: Some(number),
            disc: Some(1),
            album_track_count: None,
            codec: None,
            bit_depth: None,
            sample_rate: None,
            duration: None,
            artwork: Vec::new(),
            playback: PlaybackState::default(),
        })
    }

    fn tracks_of(&self, album: u32) -> Option<Vec<Item>> {
        let albums = self.albums.lock();
        let (_, count) = albums.iter().find(|(k, _)| *k == album)?;
        Some((1..=*count).map(|n| Self::track(album, n)).collect())
    }

    fn found(item: Item, children: Vec<Item>) -> ItemWithChildren {
        ItemWithChildren {
            external_ids: vec![format!("test://{}", item.id())],
            item,
            children,
            markers: Vec::new(),
        }
    }
}

#[async_trait]
impl Catalog for Music {
    async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
        Ok(vec![Self::library()])
    }

    async fn items(&self, _: &LibraryId, page: Page) -> Result<ItemPage, CatalogError> {
        let items = if page.offset == 0 && page.limit > 0 {
            vec![Self::artist()]
        } else {
            Vec::new()
        };
        Ok(ItemPage { items, total: 1 })
    }

    async fn item(&self, id: &MediaId) -> Result<ItemWithChildren, CatalogError> {
        let missing = || CatalogError::NotFound(id.to_string());
        let key: u32 = id.key().parse().map_err(|_| missing())?;
        match id.kind() {
            MediaKind::Artist if key == 1 => {
                let albums = self
                    .albums
                    .lock()
                    .iter()
                    .map(|(k, _)| Self::album(*k))
                    .collect();
                Ok(Self::found(Self::artist(), albums))
            }
            MediaKind::Album => {
                let tracks = self.tracks_of(key).ok_or_else(missing)?;
                Ok(Self::found(Self::album(key), tracks))
            }
            MediaKind::Track => {
                let album = key / 100 * 100;
                let tracks = self.tracks_of(album).ok_or_else(missing)?;
                let track = tracks
                    .into_iter()
                    .find(|t| t.id() == id)
                    .ok_or_else(missing)?;
                Ok(Self::found(track, Vec::new()))
            }
            _ => Err(missing()),
        }
    }

    async fn external_ids(&self, id: &MediaId) -> Result<Vec<String>, CatalogError> {
        Ok(self.item(id).await?.external_ids)
    }

    async fn search(&self, _: &SearchQuery) -> Result<Found, CatalogError> {
        Ok(Found { groups: Vec::new() })
    }

    async fn person_items(&self, id: &PersonId) -> Result<Vec<Item>, CatalogError> {
        Err(CatalogError::NotFound(id.to_string()))
    }

    /// Every track's file is there.
    async fn media_path(&self, id: &MediaId) -> Result<PathBuf, CatalogError> {
        self.item(id).await?;
        Ok(PathBuf::from(format!("/music/{}.flac", id.key())))
    }

    async fn recently_added(&self, _: u32) -> Result<Vec<Item>, CatalogError> {
        Ok(Vec::new())
    }

    async fn authorize_artwork(&self, _: &ArtworkId) -> Result<(), CatalogError> {
        Ok(())
    }

    async fn artwork(&self, id: &ArtworkId) -> Result<Image, CatalogError> {
        Err(CatalogError::NotFound(id.to_string()))
    }
}

/// Hides track 103 from one user, as a later per-user rule might.
#[derive(Debug)]
struct NotTrack103For(Principal);

impl ItemVisibility for NotTrack103For {
    fn may_see(&self, actor: &Actor, item: &Item) -> bool {
        actor.principal != self.0 || item.id().key() != "103"
    }
}

struct Harness {
    // Declared first, so the store closes before its directory goes.
    playlists: Playlists,
    music: Arc<Music>,
    alice: Actor,
    bob: Actor,
    _dir: spoolframe_test_dir::TestDir,
}

/// Alice and Bob; Bob may not see track 103.
async fn harness(test: &str) -> Harness {
    let dir = crate::store::tests::state_dir(&format!("liked-{test}")).await;
    let store = Store::open(dir.as_ref()).await.unwrap();
    let music = Arc::new(Music::new());
    let ffprobe = fake_ffprobe(dir.as_ref(), "mkv-forced-subs.json");
    let prober = Arc::new(Prober::new(
        Tool::unconfined(ffprobe),
        Prober::DEFAULT_TIMEOUT,
    ));
    let watch = Arc::new(Watch::new(
        store.clone(),
        music.clone(),
        prober,
        WatchRules::default(),
    ));
    let mut actors = Vec::new();
    for name in ["alice", "bob"] {
        let id = store
            .add_user(name, None, "phc", Role::Consumer)
            .await
            .unwrap();
        actors.push(Actor {
            principal: Principal::LocalUser(id),
            admin: false,
        });
    }
    let playlists = Playlists::with_visibility(
        store,
        music.clone(),
        watch,
        Arc::new(NotTrack103For(actors[1].principal)),
    );
    Harness {
        playlists,
        music,
        alice: actors[0],
        bob: actors[1],
        _dir: dir,
    }
}

impl Harness {
    /// `who`'s built-in playlist `builtin`, from their list.
    async fn builtin(&self, who: &Actor, builtin: Builtin) -> PlaylistSummary {
        let (list, _) = self.playlists.list(who, true).await.unwrap();
        list.into_iter()
            .find(|p| p.builtin == Some(builtin))
            .expect("listed")
    }

    /// The keys of the items in `who`'s `builtin`, in order.
    async fn keys(&self, who: &Actor, builtin: Builtin) -> Vec<String> {
        let id = self.builtin(who, builtin).await.id;
        self.playlists
            .get(who, &id, 0, MAX_PAGE)
            .await
            .unwrap()
            .entries
            .iter()
            .map(|e| e.item.id().key().to_owned())
            .collect()
    }

    /// `who`'s heart on album `key`, as its screen shows it now.
    async fn album_heart(&self, who: &Actor, key: u32) -> Liked {
        let found = self.music.item(&id(MediaKind::Album, key)).await.unwrap();
        self.playlists
            .liked(who, &found.item, &found.children)
            .await
            .unwrap()
            .0
            .expect("an album has a heart")
    }

    async fn track_heart(&self, who: &Actor, key: u32) -> Liked {
        let found = self.music.item(&id(MediaKind::Track, key)).await.unwrap();
        self.playlists
            .liked(who, &found.item, &[])
            .await
            .unwrap()
            .0
            .expect("a track has a heart")
    }
}

fn album() -> MediaId {
    id(MediaKind::Album, 100)
}

fn track(key: u32) -> MediaId {
    id(MediaKind::Track, key)
}

/// The ticket: a 12-track album liked is 12 entries and a full heart; one
/// track unliked makes it half; the album unliked takes the other 11.
#[tokio::test]
async fn liking_an_album_likes_its_tracks_and_unliking_takes_them_out() {
    let h = harness("album").await;
    let liked = h.playlists.like(&h.alice, &[album()]).await.unwrap();
    assert_eq!(liked.builtin, Some(Builtin::Liked));
    let keys = h.keys(&h.alice, Builtin::Liked).await;
    assert_eq!(keys.len(), 12);
    assert_eq!(keys[0], "101", "in disc and track order");
    assert_eq!(h.album_heart(&h.alice, 100).await, Liked::All);

    h.playlists.unlike(&h.alice, &[track(105)]).await.unwrap();
    assert_eq!(h.album_heart(&h.alice, 100).await, Liked::Some);
    assert_eq!(h.track_heart(&h.alice, 105).await, Liked::None);
    assert_eq!(h.track_heart(&h.alice, 106).await, Liked::All);
    let found = h.music.item(&album()).await.unwrap();
    let (_, children) = h
        .playlists
        .liked(&h.alice, &found.item, &found.children)
        .await
        .unwrap();
    assert_eq!(children.unwrap().len(), 11);

    h.playlists.unlike(&h.alice, &[album()]).await.unwrap();
    assert!(h.keys(&h.alice, Builtin::Liked).await.is_empty());
    assert_eq!(h.album_heart(&h.alice, 100).await, Liked::None);
}

/// The ticket: the like was for the tracks; one the album gains later is
/// not liked, so the heart goes half.
#[tokio::test]
async fn a_track_added_to_a_liked_album_later_leaves_its_heart_half() {
    let h = harness("added-later").await;
    h.playlists.like(&h.alice, &[album()]).await.unwrap();
    h.music.add_track(100);
    assert_eq!(h.album_heart(&h.alice, 100).await, Liked::Some);
    assert_eq!(h.keys(&h.alice, Builtin::Liked).await.len(), 12);
}

#[tokio::test]
async fn the_newest_liked_is_first_and_liking_again_changes_nothing() {
    let h = harness("order").await;
    h.playlists.like(&h.alice, &[track(103)]).await.unwrap();
    h.playlists.like(&h.alice, &[track(107)]).await.unwrap();
    let before = h.builtin(&h.alice, Builtin::Liked).await.revision;
    h.playlists.like(&h.alice, &[track(103)]).await.unwrap();
    assert_eq!(h.keys(&h.alice, Builtin::Liked).await, ["107", "103"]);
    assert_eq!(h.builtin(&h.alice, Builtin::Liked).await.revision, before);
    // A whole album goes on top, its tracks liked already left in place.
    h.playlists.like(&h.alice, &[album()]).await.unwrap();
    let keys = h.keys(&h.alice, Builtin::Liked).await;
    assert_eq!(keys.len(), 12);
    assert_eq!(&keys[10..], ["107", "103"]);
}

#[tokio::test]
async fn liked_is_reordered_by_its_owner() {
    let h = harness("move").await;
    h.playlists
        .like(&h.alice, &[track(101), track(102)])
        .await
        .unwrap();
    let liked = h.builtin(&h.alice, Builtin::Liked).await;
    let page = h.playlists.get(&h.alice, &liked.id, 0, 10).await.unwrap();
    assert!(page.entries.iter().all(|e| e.liked));
    let (first, second) = (page.entries[0].entry, page.entries[1].entry);
    h.playlists
        .edit(
            &h.alice,
            &liked.id,
            liked.revision,
            vec![PlaylistEdit::Move {
                entry: second,
                before: Some(first),
            }],
        )
        .await
        .unwrap();
    assert_eq!(h.keys(&h.alice, Builtin::Liked).await, ["102", "101"]);
}

/// The ticket: Liked and All are never renamed or deleted, and All is not
/// edited; a track gets into Liked only by a like.
#[tokio::test]
async fn liked_and_all_cannot_be_renamed_deleted_or_all_edited() {
    let h = harness("fixed").await;
    for builtin in [Builtin::Liked, Builtin::All] {
        let playlist = h.builtin(&h.alice, builtin).await;
        assert!(!playlist.can_manage, "{builtin:?}");
        let renamed = h
            .playlists
            .update(
                &h.alice,
                &playlist.id,
                PlaylistUpdate {
                    name: Some("Mine".into()),
                    ..PlaylistUpdate::default()
                },
            )
            .await;
        assert!(
            matches!(renamed, Err(PlaylistsError::ReadOnly)),
            "{builtin:?}"
        );
        let deleted = h.playlists.delete(&h.alice, &playlist.id).await;
        assert!(
            matches!(deleted, Err(PlaylistsError::ReadOnly)),
            "{builtin:?}"
        );
    }
    let all = h.builtin(&h.alice, Builtin::All).await;
    assert!(!all.can_edit);
    let edited = h
        .playlists
        .edit(
            &h.alice,
            &all.id,
            all.revision,
            vec![PlaylistEdit::Remove {
                entry: policy::all_entry("plex:track:101"),
            }],
        )
        .await;
    assert!(matches!(edited, Err(PlaylistsError::ReadOnly)));
    let liked = h.builtin(&h.alice, Builtin::Liked).await;
    assert!(liked.can_edit);
    let added = h
        .playlists
        .edit(
            &h.alice,
            &liked.id,
            liked.revision,
            vec![PlaylistEdit::Add {
                id: track(101),
                before: None,
            }],
        )
        .await;
    assert!(matches!(added, Err(PlaylistsError::InvalidInput(_))));
}

#[tokio::test]
async fn only_tracks_and_albums_are_liked() {
    let h = harness("kinds").await;
    let artist = h
        .playlists
        .like(&h.alice, &[id(MediaKind::Artist, 1)])
        .await;
    assert!(matches!(
        artist,
        Err(PlaylistsError::NotLikeable(MediaKind::Artist))
    ));
    let film = h
        .playlists
        .unlike(&h.alice, &[id(MediaKind::Movie, 1)])
        .await;
    assert!(matches!(
        film,
        Err(PlaylistsError::NotLikeable(MediaKind::Movie))
    ));
}

/// The ticket: one user's likes are not another's.
#[tokio::test]
async fn one_user_s_likes_are_not_another_s() {
    let h = harness("per-user").await;
    h.playlists.like(&h.alice, &[album()]).await.unwrap();
    assert_eq!(h.album_heart(&h.bob, 100).await, Liked::None);
    assert!(h.keys(&h.bob, Builtin::Liked).await.is_empty());
    let alices = h.builtin(&h.alice, Builtin::Liked).await;
    assert!(matches!(
        h.playlists.get(&h.bob, &alices.id, 0, 10).await,
        Err(PlaylistsError::NotFound)
    ));
}

/// The ticket: All holds every track the user may see and none they may
/// not, and a new album is in it after a scan with no edit.
#[tokio::test]
async fn all_is_every_track_one_may_see_and_follows_a_scan() {
    let h = harness("all").await;
    assert_eq!(h.keys(&h.alice, Builtin::All).await.len(), 12);
    let bobs = h.keys(&h.bob, Builtin::All).await;
    assert_eq!(bobs.len(), 11);
    assert!(!bobs.contains(&"103".to_owned()));

    let new = h.music.add_album(3);
    let all = h.builtin(&h.alice, Builtin::All).await;
    let page = h
        .playlists
        .get(&h.alice, &all.id, 0, MAX_PAGE)
        .await
        .unwrap();
    assert_eq!((page.total, page.playable), (15, 15));
    assert!(
        page.entries
            .iter()
            .any(|e| e.item.id() == &id(MediaKind::Track, new + 1))
    );
    assert_eq!(page.playlist.revision, all.revision, "no edit");
}

#[tokio::test]
async fn liked_and_all_are_listed_first_and_only_to_who_takes_them() {
    let h = harness("listed").await;
    h.playlists
        .create(
            &h.alice,
            "Aardvark mix",
            Kinds {
                tracks: true,
                ..Kinds::default()
            },
            Visibility::Personal,
        )
        .await
        .unwrap();
    let (list, _) = h.playlists.list(&h.alice, true).await.unwrap();
    let order: Vec<_> = list.iter().map(|p| p.builtin).collect();
    assert_eq!(order, [Some(Builtin::Liked), Some(Builtin::All), None]);
    let (older, _) = h.playlists.list(&h.alice, false).await.unwrap();
    assert_eq!(older.len(), 1);
    assert_eq!(older[0].builtin, None);
}

/// Liked shows on an album's and its artist's screens as any playlist
/// holding a track does; All never, as it holds everything.
#[tokio::test]
async fn liked_is_among_the_playlists_holding_an_album_and_all_is_not() {
    let h = harness("holding").await;
    h.playlists.like(&h.alice, &[track(104)]).await.unwrap();
    for screen in [Music::album(100), Music::artist()] {
        let held = h
            .playlists
            .holding(&h.alice, &screen, &[], true)
            .await
            .unwrap();
        let builtins: Vec<_> = held.iter().map(|r| r.builtin).collect();
        assert_eq!(builtins, [Some(Builtin::Liked)], "{screen:?}");
        let older = h
            .playlists
            .holding(&h.alice, &screen, &[], false)
            .await
            .unwrap();
        assert!(older.is_empty());
    }
}

/// All plays as any playlist does, and its place holds after a scan adds
/// an album, as its entries are named by their tracks.
#[tokio::test]
async fn all_plays_in_order_and_goes_on_after_a_scan() {
    let h = harness("play-all").await;
    let all = h.builtin(&h.alice, Builtin::All).await;
    let first = h
        .playlists
        .play(&h.alice, &all.id, None, PlaySettings::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.item.id(), &track(101));
    let second = h
        .playlists
        .next(&h.alice, &all.id, first.entry)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.item.id(), &track(102));
    h.music.add_album(2);
    let third = h
        .playlists
        .next(&h.alice, &all.id, second.entry)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(third.item.id(), &track(103));
    let page = h.playlists.get(&h.alice, &all.id, 0, 1).await.unwrap();
    assert_eq!(page.place, Some(third.entry));
}

#[tokio::test]
async fn shuffle_on_all_plays_the_whole_library_once_a_round() {
    let h = harness("shuffle-all").await;
    let all = h.builtin(&h.alice, Builtin::All).await;
    let shuffled = PlaySettings {
        shuffle: true,
        repeat: Repeat::All,
        skip_watched: false,
    };
    let mut next = h
        .playlists
        .play(&h.alice, &all.id, None, shuffled)
        .await
        .unwrap();
    let mut played = Vec::new();
    for _ in 0..12 {
        let now = next.expect("repeat all never ends");
        played.push(now.item.id().key().to_owned());
        next = h
            .playlists
            .next(&h.alice, &all.id, now.entry)
            .await
            .unwrap();
    }
    played.sort();
    let every: Vec<String> = (101..=112).map(|k| k.to_string()).collect();
    assert_eq!(played, every);
}

#[tokio::test]
async fn the_track_playing_from_liked_has_its_heart() {
    let h = harness("play-liked").await;
    h.playlists.like(&h.alice, &[track(102)]).await.unwrap();
    let liked = h.builtin(&h.alice, Builtin::Liked).await;
    let playing = h
        .playlists
        .play(&h.alice, &liked.id, None, PlaySettings::default())
        .await
        .unwrap()
        .unwrap();
    assert!(playing.liked);
}

/// A Plex import's rated tracks: each not liked yet goes to the top of
/// Liked, in the order given; one liked already stays; a dry run likes
/// nothing; what is not here, or not a track, is reported.
#[tokio::test]
async fn an_import_likes_the_rated_tracks_here() {
    let h = harness("import").await;
    h.playlists.like(&h.alice, &[track(110)]).await.unwrap();
    let rated = vec![
        (track(102), "Track 2".to_owned()),
        (track(110), "Track 10".to_owned()),
        (track(101), "Track 1".to_owned()),
        (track(999), "Gone".to_owned()),
        (album(), "An album".to_owned()),
    ];
    let (none, count, left_out) = h
        .playlists
        .import_liked(&h.alice, &rated, true)
        .await
        .unwrap();
    assert_eq!((none, count), (None, 2));
    assert_eq!(left_out.len(), 2);
    assert_eq!(h.keys(&h.alice, Builtin::Liked).await, ["110"]);
    let (made, count, _) = h
        .playlists
        .import_liked(&h.alice, &rated, false)
        .await
        .unwrap();
    assert_eq!(made, Some(h.builtin(&h.alice, Builtin::Liked).await.id));
    assert_eq!(count, 2);
    assert_eq!(
        h.keys(&h.alice, Builtin::Liked).await,
        ["102", "101", "110"]
    );
}
