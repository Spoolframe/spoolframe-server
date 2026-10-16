//! Playlist covers (PX-53) through [`Playlists`] itself: who may set one,
//! what every viewer sees, and the mosaic of the first four. The fake
//! catalog gives each film, season and album a poster of its own; covers
//! are decoded by real ffmpeg.

use std::sync::Arc;

use spoolframe_media::Prober;
use spoolframe_media::sandbox::Tool;
use spoolframe_protocol::playlists::{Builtin, Kinds, PlaylistEdit, Visibility};
use spoolframe_types::{MediaId, MediaKind, Namespace, Role};

use super::*;
use crate::covers::{self, Covers, Tile};
use crate::principal::{Actor, Principal};
use crate::test_support::{FakeCatalog, fake_ffprobe, poster_of};
use crate::watch::{Watch, WatchRules};

const EVERYTHING: Kinds = Kinds {
    movies: true,
    episodes: true,
    tracks: true,
};

struct Harness {
    playlists: Playlists,
    catalog: Arc<FakeCatalog>,
    covers: Arc<Covers>,
    alice: Actor,
    bob: Actor,
    carol: Actor,
    _dir: spoolframe_test_dir::TestDir,
}

async fn harness(test: &str) -> Harness {
    let dir = crate::store::tests::state_dir(&format!("playlist-art-{test}")).await;
    let store = Store::open(dir.as_ref()).await.unwrap();
    let catalog = Arc::new(FakeCatalog::new(dir.join("media").join("movie.mkv")));
    catalog.show_posters();
    let prober = Arc::new(Prober::new(
        Tool::unconfined(fake_ffprobe(dir.as_ref(), "mkv-forced-subs.json")),
        Prober::DEFAULT_TIMEOUT,
    ));
    let watch = Arc::new(Watch::new(
        store.clone(),
        catalog.clone(),
        prober,
        WatchRules::default(),
    ));
    let mut actors = Vec::new();
    for name in ["alice", "bob", "carol"] {
        let id = store
            .add_user(name, None, "phc", Role::Consumer)
            .await
            .unwrap();
        actors.push(Actor {
            principal: Principal::LocalUser(id),
            admin: false,
        });
    }
    let covers = Arc::new(Covers::new(dir.as_ref(), Tool::unconfined("ffmpeg")));
    let playlists = Playlists::new(store, catalog.clone(), watch).with_covers(covers.clone());
    Harness {
        playlists,
        catalog,
        covers,
        alice: actors[0],
        bob: actors[1],
        carol: actors[2],
        _dir: dir,
    }
}

fn id(kind: MediaKind, key: &str) -> MediaId {
    MediaId::new(Namespace::Plex, kind, key).unwrap()
}

fn picture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../spoolframe-media/tests/fixtures/pictures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

impl Harness {
    /// Alice's playlist of `items`, in order, shared as `visibility`.
    async fn made(&self, items: &[MediaId], visibility: Visibility) -> String {
        let made = self
            .playlists
            .create(&self.alice, "Road trip", EVERYTHING, visibility)
            .await
            .unwrap();
        if !items.is_empty() {
            let edits = items
                .iter()
                .map(|item| PlaylistEdit::Add {
                    id: item.clone(),
                    before: None,
                })
                .collect();
            self.playlists
                .edit(&self.alice, &made.id, made.revision, edits)
                .await
                .unwrap();
        }
        made.id
    }

    /// Playlist `id` as `who` sees it.
    async fn seen(&self, who: &Actor, id: &str) -> PlaylistSummary {
        self.playlists.get(who, id, 0, 0).await.unwrap().playlist
    }

    async fn art(&self, who: &Actor, id: &str) -> ArtworkId {
        self.seen(who, id).await.artwork.unwrap()
    }

    fn shared_with(&self, who: &Actor, can_edit: bool) -> Visibility {
        Visibility::Shared {
            with: vec![spoolframe_protocol::playlists::PlaylistShare::new(
                who.principal.to_string(),
                can_edit,
            )],
        }
    }
}

/// Five covers (two films, two seasons' episodes, an album's track): the
/// mosaic is of the first four; the fifth moved to the top changes it.
#[tokio::test]
async fn the_mosaic_is_the_first_four_and_follows_the_order() {
    let h = harness("mosaic").await;
    let playlist = h
        .made(
            &[
                id(MediaKind::Movie, "1"),
                id(MediaKind::Movie, "2"),
                id(MediaKind::Episode, "12"),
                // The same season's: no cover of its own.
                id(MediaKind::Episode, "13"),
                id(MediaKind::Episode, "15"),
                id(MediaKind::Track, "32"),
            ],
            Visibility::Personal,
        )
        .await;
    let first_four = ["1", "2", "11", "14"].map(poster_of);
    assert_eq!(
        h.art(&h.alice, &playlist).await,
        covers::mosaic_id(&first_four)
    );
    assert!(!h.seen(&h.alice, &playlist).await.custom_art);
    let page = h.playlists.get(&h.alice, &playlist, 0, 10).await.unwrap();
    let track = page.entries.last().unwrap().entry;
    let top = page.entries.first().unwrap().entry;
    h.playlists
        .edit(
            &h.alice,
            &playlist,
            page.playlist.revision,
            vec![PlaylistEdit::Move {
                entry: track,
                before: Some(top),
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        h.art(&h.alice, &playlist).await,
        covers::mosaic_id(&["31", "1", "2", "11"].map(poster_of))
    );
}

/// One film's poster alone; one album's tracks, one cover; nothing, the
/// placeholder.
#[tokio::test]
async fn fewer_than_four_covers_show_the_first_and_none_the_placeholder() {
    let h = harness("few").await;
    let film = h
        .made(&[id(MediaKind::Movie, "1")], Visibility::Personal)
        .await;
    assert_eq!(h.art(&h.alice, &film).await, poster_of("1"));
    let album = h
        .made(
            &[id(MediaKind::Track, "32"), id(MediaKind::Track, "33")],
            Visibility::Personal,
        )
        .await;
    assert_eq!(h.art(&h.alice, &album).await, poster_of("31"));
    let empty = h.made(&[], Visibility::Personal).await;
    assert_eq!(h.art(&h.alice, &empty).await, covers::tile_id(Tile::Empty));
    // The listing says the same.
    let (listed, _) = h.playlists.list(&h.alice, false).await.unwrap();
    let art: Vec<_> = listed.iter().map(|p| p.artwork.clone().unwrap()).collect();
    assert_eq!(
        art,
        [
            poster_of("1"),
            poster_of("31"),
            covers::tile_id(Tile::Empty)
        ]
    );
}

/// An editor's upload is what everyone who sees the playlist sees; taken
/// away, the mosaic is back and the file goes.
#[tokio::test]
async fn an_editor_s_cover_is_everyone_s_and_removing_it_brings_the_mosaic_back() {
    let h = harness("upload").await;
    let shared = Visibility::Shared {
        with: vec![
            spoolframe_protocol::playlists::PlaylistShare::new(h.bob.principal.to_string(), true),
            spoolframe_protocol::playlists::PlaylistShare::new(
                h.carol.principal.to_string(),
                false,
            ),
        ],
    };
    let playlist = h.made(&[id(MediaKind::Movie, "1")], shared).await;
    let set = h
        .playlists
        .set_art(&h.bob, &playlist, &picture("photo.heic"))
        .await
        .unwrap();
    let cover = set.artwork.clone().unwrap();
    assert!(matches!(
        covers::local(&cover),
        Some(covers::Local::Cover(_))
    ));
    assert!(set.custom_art);
    for who in [&h.alice, &h.bob, &h.carol] {
        let seen = h.seen(who, &playlist).await;
        assert_eq!(seen.artwork.as_ref(), Some(&cover));
        assert!(seen.custom_art);
        assert!(h.playlists.sees_art(who, &cover).await.unwrap());
    }
    assert!(h.covers.has(&cover).await);
    let removed = h.playlists.remove_art(&h.alice, &playlist).await.unwrap();
    assert_eq!(removed.artwork, Some(poster_of("1")));
    assert!(!removed.custom_art);
    assert_eq!(h.art(&h.carol, &playlist).await, poster_of("1"));
    assert!(!h.covers.has(&cover).await, "no playlist uses it");
    assert!(!h.playlists.sees_art(&h.carol, &cover).await.unwrap());
}

/// One picture on two playlists is one file, kept while either has it,
/// and gone with the last: by removal, or by deleting the playlist.
#[tokio::test]
async fn a_cover_s_file_goes_with_the_last_playlist_that_has_it() {
    let h = harness("shared-file").await;
    let one = h.made(&[], Visibility::Personal).await;
    let two = h.made(&[], Visibility::Personal).await;
    let a = h
        .playlists
        .set_art(&h.alice, &one, &picture("tall.png"))
        .await
        .unwrap()
        .artwork
        .unwrap();
    let b = h
        .playlists
        .set_art(&h.alice, &two, &picture("tall.png"))
        .await
        .unwrap()
        .artwork
        .unwrap();
    assert_eq!(a, b);
    h.playlists.remove_art(&h.alice, &one).await.unwrap();
    assert!(h.covers.has(&a).await);
    h.playlists.delete(&h.alice, &two).await.unwrap();
    assert!(!h.covers.has(&a).await);
}

/// A read-only share sees the cover and may not change it; nor may
/// anyone set one on Liked or All, whose tiles are fixed.
#[tokio::test]
async fn a_viewer_and_liked_and_all_are_not_allowed() {
    let h = harness("refused").await;
    let shared = h.shared_with(&h.bob, false);
    let playlist = h.made(&[], shared).await;
    assert!(matches!(
        h.playlists
            .set_art(&h.bob, &playlist, &picture("tall.png"))
            .await,
        Err(PlaylistsError::NotAllowed)
    ));
    assert!(matches!(
        h.playlists.remove_art(&h.bob, &playlist).await,
        Err(PlaylistsError::NotAllowed)
    ));
    // Carol does not see it at all.
    assert!(matches!(
        h.playlists
            .set_art(&h.carol, &playlist, &picture("tall.png"))
            .await,
        Err(PlaylistsError::NotFound)
    ));
    h.catalog.serve_music();
    let (listed, _) = h.playlists.list(&h.alice, true).await.unwrap();
    for (builtin, tile) in [(Builtin::Liked, Tile::Liked), (Builtin::All, Tile::All)] {
        let p = listed.iter().find(|p| p.builtin == Some(builtin)).unwrap();
        assert_eq!(p.artwork, Some(covers::tile_id(tile)));
        assert!(matches!(
            h.playlists
                .set_art(&h.alice, &p.id, &picture("tall.png"))
                .await,
            Err(PlaylistsError::NotAllowed)
        ));
        assert!(matches!(
            h.playlists.remove_art(&h.alice, &p.id).await,
            Err(PlaylistsError::NotAllowed)
        ));
    }
}

#[tokio::test]
async fn too_large_and_unreadable_pictures_change_nothing() {
    let h = harness("bad-pictures").await;
    let playlist = h.made(&[], Visibility::Personal).await;
    let mut big = picture("tall.png");
    big.resize(25 << 20, 0);
    assert!(matches!(
        h.playlists.set_art(&h.alice, &playlist, &big).await,
        Err(PlaylistsError::ImageTooLarge)
    ));
    assert!(matches!(
        h.playlists
            .set_art(&h.alice, &playlist, b"%PDF-1.7 not a picture")
            .await,
        Err(PlaylistsError::ImageUnreadable(_))
    ));
    let seen = h.seen(&h.alice, &playlist).await;
    assert!(!seen.custom_art);
    assert_eq!(seen.artwork, Some(covers::tile_id(Tile::Empty)));
}

#[test]
fn the_protocol_s_limits_are_the_decoder_s() {
    assert_eq!(
        spoolframe_media::picture::MAX_BYTES,
        spoolframe_protocol::playlists::MAX_ART_BYTES
    );
    assert_eq!(
        spoolframe_media::picture::MAX_PIXELS,
        spoolframe_protocol::playlists::MAX_ART_PIXELS
    );
}
