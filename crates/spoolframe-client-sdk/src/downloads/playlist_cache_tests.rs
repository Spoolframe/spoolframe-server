//! Playlists and hearts kept for offline, worked out with no server.

use std::sync::Arc;

use spoolframe_protocol::downloads::{DownloadFile, DownloadSource, DownloadTracks};
use spoolframe_protocol::playlists::{
    Builtin, Kinds, PlaySettings, PlaylistEntry, PlaylistOwnerKind, PlaylistPage, PlaylistSummary,
    Visibility,
};
use spoolframe_types::{
    ArtworkId, Item, MediaId, MediaKind, MediaTracks, Namespace, PlaybackState, Track,
};

use super::super::local_db::Local;
use super::super::music::{MusicTarget, TrackOnDevice};
use super::super::store::{EntryKind, Hold, Store};
use super::super::{DownloadPhase, LocalWatch, Shared};
use super::*;
use crate::PlaylistList;

fn server() -> EndpointId {
    iroh::SecretKey::from_bytes(&[7; 32]).public()
}

fn track(key: &str) -> Item {
    Item::Track(Track {
        id: MediaId::new(Namespace::Plex, MediaKind::Track, key).unwrap(),
        album_id: MediaId::new(Namespace::Plex, MediaKind::Album, "31").unwrap(),
        artist_id: MediaId::new(Namespace::Plex, MediaKind::Artist, "30").unwrap(),
        artist_name: Some("Röyksopp".into()),
        album_title: Some("Melody A.M.".into()),
        title: format!("Track {key}"),
        track: None,
        disc: None,
        album_track_count: None,
        codec: None,
        bit_depth: None,
        sample_rate: None,
        duration: None,
        artwork: Vec::new(),
        playback: PlaybackState::default(),
    })
}

fn summary(id: &str, name: &str, revision: u64) -> PlaylistSummary {
    PlaylistSummary {
        id: id.into(),
        name: name.into(),
        kinds: Kinds::default(),
        owner_kind: PlaylistOwnerKind::Mine,
        visibility: Visibility::Personal,
        can_edit: true,
        can_manage: true,
        revision,
        pinned: None,
        builtin: None,
        owner_name: None,
        artwork: None,
        custom_art: false,
    }
}

fn list(summaries: Vec<PlaylistSummary>) -> PlaylistList {
    PlaylistList {
        playlists: summaries,
        pin_notes: Vec::new(),
    }
}

/// `summary`'s whole page: these tracks, `liked` the ones liked.
fn page(summary: PlaylistSummary, keys: &[&str], liked: &[&str]) -> PlaylistPage {
    PlaylistPage {
        playlist: summary,
        entries: keys
            .iter()
            .zip(1..)
            .map(|(key, entry)| PlaylistEntry {
                entry,
                item: track(key),
                unavailable: false,
                liked: liked.contains(key),
            })
            .collect(),
        offset: 0,
        total: keys.len() as u32,
        playable: keys.len() as u32,
        place: None,
        settings: PlaySettings::default(),
    }
}

struct Device {
    shared: Arc<Shared>,
    store: Store,
}

fn device(test: &str) -> Device {
    let dir = std::env::temp_dir().join(format!(
        "sf-sdk-playlist-cache-{test}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let shared = Shared::new(Some(dir));
    // The client's own store: what is written there is indexed.
    let store = shared.store.clone().unwrap();
    Device { shared, store }
}

impl Device {
    fn server(&self) -> String {
        server().to_string()
    }

    /// Track `key` here whole for "user", held by `holds`.
    fn track_here(&self, key: &str, holds: Vec<Hold>) {
        let mut m = Store::manifest(
            EntryKind::Track,
            // Its key as a download of it has it: found by its id.
            super::super::music::track_key(
                &self.server(),
                track(key).id(),
                &DownloadSource::Original,
            ),
            self.server(),
            "user".into(),
            "alice".into(),
            track(key),
            MediaTracks::default(),
            DownloadSource::Original,
            DownloadTracks::All,
            LocalWatch::default(),
            1,
        );
        m.holds = holds;
        m.size = Some(3);
        m.offset = 3;
        m.phase = DownloadPhase::Done;
        m.file = Some(DownloadFile {
            container: spoolframe_types::Container::Flac,
            tracks: Default::default(),
        });
        // The file first, as a download finishes: then its manifest.
        let media = self.store.media(&m).unwrap();
        std::fs::create_dir_all(media.parent().unwrap()).unwrap();
        std::fs::write(media, b"abc").unwrap();
        self.store.write(&m).unwrap();
    }

    fn listed(&self, summaries: Vec<PlaylistSummary>) {
        self.shared.playlists_listed(
            &self.shared.writer(server(), "user", "alice"),
            &list(summaries),
        );
    }

    fn queued(&self) -> Vec<Job> {
        self.shared.flush_db();
        self.shared.playlists.queued()
    }

    fn clear(&self) {
        self.shared.flush_db();
        self.shared.playlists.clear();
    }

    /// Run `query` on `user`'s database.
    fn on_db<R: Send + 'static>(
        &self,
        user: &str,
        query: impl FnOnce(&mut rusqlite::Connection) -> R + Send + 'static,
    ) -> R {
        let (s, u) = (self.server(), user.to_owned());
        self.shared
            .dbs
            .as_ref()
            .unwrap()
            .call(move |local: &mut Local| query(local.db(&s, &u).unwrap()))
            .unwrap()
    }

    fn hearts_rows(&self, user: &str) -> i64 {
        self.on_db(user, |conn| {
            conn.query_row("SELECT count(*) FROM hearts", [], |r| r.get(0))
                .unwrap()
        })
    }
}

#[test]
fn after_the_list_is_read_each_playlist_never_read_is_queued() {
    let device = device("queued");
    device.listed(vec![summary("p1", "Road Trip", 1), summary("p2", "Gym", 4)]);
    assert_eq!(
        device.queued(),
        [Job::Playlist("p1".into()), Job::Playlist("p2".into())]
    );
}

#[test]
fn a_playlist_read_at_its_revision_is_not_read_again_and_a_changed_one_is() {
    let device = device("revision");
    device.listed(vec![summary("p1", "Road Trip", 1)]);
    device.clear();
    device.shared.playlist_read(
        &device.shared.writer(server(), "user", "alice"),
        &page(summary("p1", "Road Trip", 1), &["1"], &[]),
    );
    device.listed(vec![summary("p1", "Road Trip", 1)]);
    let unchanged = device.queued();
    device.listed(vec![summary("p1", "Road Trip", 2)]);
    assert_eq!(
        (unchanged, device.queued()),
        (vec![], vec![Job::Playlist("p1".into())])
    );
}

#[test]
fn a_changed_cover_is_kept_again() {
    let device = device("cover");
    let mut p = summary("p1", "Road Trip", 1);
    device.listed(vec![p.clone()]);
    device.shared.playlist_read(
        &device.shared.writer(server(), "user", "alice"),
        &page(p.clone(), &["1"], &[]),
    );
    device.clear();
    p.artwork = Some("plex:art:1".parse::<ArtworkId>().unwrap());
    device.listed(vec![p]);
    assert_eq!(device.queued(), [Job::Playlist("p1".into())]);
}

#[test]
fn an_event_of_a_changed_playlist_queues_it_and_of_an_unknown_one_the_list() {
    let device = device("events");
    device.listed(vec![summary("p1", "Road Trip", 1)]);
    device.shared.playlist_read(
        &device.shared.writer(server(), "user", "alice"),
        &page(summary("p1", "Road Trip", 1), &["1"], &[]),
    );
    device.clear();
    device.shared.playlist_changed(server(), "p1", Some(1));
    let same = device.queued();
    device.shared.playlist_changed(server(), "p1", Some(2));
    device.shared.playlist_changed(server(), "p9", Some(1));
    device.shared.playlist_changed(server(), "p1", None);
    assert_eq!(
        (same, device.queued()),
        (vec![], vec![Job::Playlist("p1".into()), Job::List])
    );
}

#[test]
fn offline_every_playlist_is_listed_with_how_many_of_its_tracks_are_here() {
    let device = device("offline-list");
    device.track_here(
        "1",
        vec![
            MusicTarget::Album {
                id: MediaId::new(Namespace::Plex, MediaKind::Album, "31").unwrap(),
            }
            .hold("user"),
        ],
    );
    device.track_here("2", vec![Hold::listening()]);
    device.listed(vec![summary("p1", "Road Trip", 1), summary("p2", "Gym", 1)]);
    device.shared.playlist_read(
        &device.shared.writer(server(), "user", "alice"),
        &page(summary("p1", "Road Trip", 1), &["1", "2", "3"], &[]),
    );
    device.shared.playlist_read(
        &device.shared.writer(server(), "user", "alice"),
        &page(summary("p2", "Gym", 1), &["4", "5"], &[]),
    );
    let music = device.shared.offline_music(&device.server(), "user");
    let shown: Vec<(String, u32, u32, Vec<TrackOnDevice>)> = music
        .playlists
        .iter()
        .map(|p| {
            (
                p.name.clone(),
                p.on_device,
                p.total,
                p.entries.iter().map(|e| e.on_device).collect(),
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            (
                "Road Trip".to_owned(),
                2,
                3,
                vec![
                    TrackOnDevice::Downloaded,
                    TrackOnDevice::Kept,
                    TrackOnDevice::Absent
                ]
            ),
            (
                "Gym".to_owned(),
                0,
                2,
                vec![TrackOnDevice::Absent, TrackOnDevice::Absent]
            ),
        ]
    );
}

#[test]
fn offline_a_track_s_heart_is_known_from_what_was_read() {
    let device = device("hearts");
    device.track_here("1", vec![Hold::download("user")]);
    device.track_here("2", vec![Hold::download("user")]);
    let mut liked = summary("liked", "Liked", 3);
    liked.builtin = Some(Builtin::Liked);
    device.listed(vec![summary("p1", "Road Trip", 1), liked.clone()]);
    device.shared.playlist_read(
        &device.shared.writer(server(), "user", "alice"),
        &page(summary("p1", "Road Trip", 1), &["1", "2"], &["1", "2"]),
    );
    // Liked read whole later holds only 1: 2 was unliked.
    device.shared.playlist_read(
        &device.shared.writer(server(), "user", "alice"),
        &page(liked, &["1"], &["1"]),
    );
    let music = device.shared.offline_music(&device.server(), "user");
    let mut hearts: Vec<(String, Option<bool>)> = music.artists[0].albums[0]
        .tracks
        .iter()
        .map(|t| (t.item.id().to_string(), t.liked))
        .collect();
    hearts.sort();
    let road_trip: Vec<Option<bool>> = music.playlists[0].entries.iter().map(|e| e.liked).collect();
    assert_eq!(
        (hearts, road_trip),
        (
            vec![
                ("plex:track:1".to_owned(), Some(true)),
                ("plex:track:2".to_owned(), Some(false))
            ],
            vec![Some(true), Some(false)]
        )
    );
}

#[test]
fn a_playlist_no_longer_listed_is_dropped() {
    let device = device("dropped");
    device.listed(vec![summary("p1", "Road Trip", 1), summary("p2", "Gym", 1)]);
    device.listed(vec![summary("p2", "Gym", 1)]);
    let music = device.shared.offline_music(&device.server(), "user");
    let names: Vec<&str> = music.playlists.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["Gym"]);
}

#[test]
fn a_profile_sees_only_its_own_playlists() {
    let device = device("profiles");
    device.shared.playlists_listed(
        &device.shared.writer(server(), "bob", "bob"),
        &list(vec![summary("p1", "Bob's", 1)]),
    );
    let music = device.shared.offline_music(&device.server(), "user");
    assert!(music.playlists.is_empty());
}

#[tokio::test]
async fn forgetting_a_profile_removes_its_playlists_and_hearts() {
    let device = device("forget");
    device.listed(vec![summary("p1", "Road Trip", 1)]);
    device.shared.playlist_read(
        &device.shared.writer(server(), "user", "alice"),
        &page(summary("p1", "Road Trip", 1), &["1"], &["1"]),
    );
    device.shared.playlists_listed(
        &device.shared.writer(server(), "bob", "bob"),
        &list(vec![summary("p2", "Bob's", 1)]),
    );
    device
        .shared
        .forget_playlists(&device.server(), Some("alice"))
        .await;
    let alice = device.shared.offline_music(&device.server(), "user");
    let bob = device.shared.offline_music(&device.server(), "bob");
    assert_eq!((alice.playlists.len(), bob.playlists.len()), (0, 1));
    assert_eq!(device.hearts_rows("user"), 0);
    device.shared.forget_playlists(&device.server(), None).await;
    let bob = device.shared.offline_music(&device.server(), "bob");
    assert!(bob.playlists.is_empty());
}

#[test]
fn offline_search_finds_a_playlist_with_nothing_here() {
    let device = device("search");
    device.listed(vec![
        summary("p1", "Road Trip", 1),
        summary("p2", "Café Gym", 1),
    ]);
    device.shared.playlist_read(
        &device.shared.writer(server(), "user", "alice"),
        &page(summary("p2", "Café Gym", 1), &["4"], &[]),
    );
    let found = super::super::offline_search::search_playlists(
        &device.shared,
        &device.server(),
        "user",
        "cafe",
    )
    .unwrap();
    let shown: Vec<(&str, u32, u32)> = found
        .iter()
        .map(|p| (p.name.as_str(), p.on_device, p.total))
        .collect();
    assert_eq!(shown, [("Café Gym", 0, 1)]);
}

/// A playlist of any length is kept whole and read a page at a time.
#[test]
fn a_playlist_of_200k_entries_is_kept_and_paged() {
    const N: u32 = 200_000;
    let device = device("huge");
    device.listed(vec![summary("all", "All", 1)]);
    let shown = summary("all", "All", 1);
    let dbs = device.shared.dbs.clone().unwrap();
    let server = device.server();
    dbs.call(move |local: &mut Local| {
        let conn = local.db(&server, "user").unwrap();
        for start in (0..N).step_by(20_000) {
            let entries: Vec<PlaylistEntry> = (start..start + 20_000)
                .map(|n| PlaylistEntry {
                    entry: u64::from(n),
                    item: track(&n.to_string()),
                    unavailable: false,
                    liked: false,
                })
                .collect();
            let tx = conn.transaction().unwrap();
            super::super::local_db::stage(&tx, "all", start, &entries).unwrap();
            tx.commit().unwrap();
        }
        super::super::local_db::commit_staged(conn, &shown, N, None).unwrap();
    })
    .unwrap();
    let music = device.shared.offline_music(&device.server(), "user");
    let all = &music.playlists[0];
    let last = device
        .shared
        .playlist_entries(&device.server(), "user", "all", N - 10, 20);
    let keys: Vec<String> = last.iter().map(|e| e.item.id().key().to_owned()).collect();
    assert_eq!(
        (
            all.total,
            all.entries.len() as u32,
            keys.len(),
            keys[0].as_str()
        ),
        (N, INLINE_ENTRIES, 10, "199990")
    );
}

/// A heart changed is one row written; one learned again as it was is
/// none.
#[test]
fn a_heart_toggled_writes_one_row() {
    let device = device("heart-row");
    let id = |key: &str| track(key).id().clone();
    device.shared.learn_hearts(
        &device.shared.writer(server(), "user", "alice"),
        [(id("1"), true), (id("2"), true), (id("3"), false)],
    );
    device.shared.flush_db();
    let before = device.on_db("user", |conn| conn.total_changes());
    device.shared.learn_hearts(
        &device.shared.writer(server(), "user", "alice"),
        [(id("1"), false), (id("2"), true), (id("3"), false)],
    );
    device.shared.flush_db();
    let after = device.on_db("user", |conn| conn.total_changes());
    let liked: Vec<(String, bool)> = device.on_db("user", |conn| {
        let mut stmt = conn
            .prepare("SELECT track, liked FROM hearts ORDER BY track")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    });
    assert_eq!(
        (after - before, liked),
        (
            1,
            vec![
                ("plex:track:1".to_owned(), false),
                ("plex:track:2".to_owned(), true),
                ("plex:track:3".to_owned(), false)
            ]
        )
    );
}

/// The JSON files of the build before are removed when the database is
/// made: the background fill reads the playlists again.
#[test]
fn the_json_files_of_the_build_before_are_removed_on_first_open() {
    let device = device("old-json");
    let dir = device.store.user_dir(&device.server(), "user");
    std::fs::create_dir_all(dir.join("playlist-art")).unwrap();
    for file in ["playlists.json", "hearts.json", "playlist-art/old.jpg"] {
        std::fs::write(dir.join(file), b"{}").unwrap();
    }
    device.listed(vec![summary("p1", "Road Trip", 1)]);
    device.shared.flush_db();
    let left: Vec<bool> = ["playlists.json", "hearts.json", "playlist-art"]
        .iter()
        .map(|f| dir.join(f).exists())
        .collect();
    assert_eq!(
        (left, dir.join("offline.sqlite3").exists()),
        (vec![false, false, false], true)
    );
}

/// A playlist renamed and read again is found by its new name offline,
/// not its old one, with no list read in between.
#[test]
fn a_renamed_playlist_is_found_by_its_new_name() {
    let device = device("renamed");
    device.listed(vec![summary("p1", "Road Trip", 1)]);
    device.shared.playlist_read(
        &device.shared.writer(server(), "user", "alice"),
        &page(summary("p1", "Gym Mix", 2), &["1"], &[]),
    );
    device.shared.flush_db();
    let found = |query: &str| {
        super::super::offline_search::search_playlists(
            &device.shared,
            &device.server(),
            "user",
            query,
        )
        .unwrap()
        .len()
    };
    assert_eq!((found("gym"), found("road")), (1, 0));
}

/// A heart changed here with no server shows the same on a playlist's
/// entries inline and in a page of them, and on the track to play.
#[test]
fn a_heart_changed_offline_shows_the_same_inline_and_paged() {
    let device = device("pending-heart");
    device.track_here("1", vec![Hold::download("user")]);
    device.listed(vec![summary("p1", "Road Trip", 1)]);
    device.shared.playlist_read(
        &device.shared.writer(server(), "user", "alice"),
        &page(summary("p1", "Road Trip", 1), &["1"], &["1"]),
    );
    device.shared.flush_db();
    let plays = device.store.offline_plays_path(&device.server(), "user");
    std::fs::write(
        &plays,
        r#"{"plays":[],"liked":[{"track":"plex:track:1","liked":false,"at":5}]}"#,
    )
    .unwrap();
    let music = device.shared.offline_music(&device.server(), "user");
    let inline = &music.playlists[0].entries[0];
    let paged = device
        .shared
        .playlist_entries(&device.server(), "user", "p1", 0, 10);
    assert_eq!(
        (
            inline.liked,
            inline.track.as_ref().and_then(|t| t.liked),
            paged[0].liked,
            paged[0].track.as_ref().and_then(|t| t.liked),
        ),
        (Some(false), Some(false), Some(false), Some(false))
    );
}

/// Pages that answer from two revisions: the playlist was edited while
/// it was read.
struct Edited {
    pages: std::sync::Mutex<Vec<PlaylistPage>>,
}

impl Pages for Edited {
    async fn page(&self, _: &str, _: u32, _: u32) -> Result<PlaylistPage, SdkError> {
        Ok(self.pages.lock().unwrap().remove(0))
    }

    async fn cover(&self, _: &ArtworkId) -> Result<Vec<u8>, SdkError> {
        Err(SdkError::NotFound("no cover".into()))
    }
}

/// Liked edited between its pages, every time it is read: nothing of it
/// is kept, and no heart is cleared by a mixed read.
#[tokio::test]
async fn a_playlist_edited_while_read_is_not_kept_mixed() {
    let device = device("mixed");
    let mut liked = summary("liked", "Liked", 1);
    liked.builtin = Some(Builtin::Liked);
    device.listed(vec![liked.clone()]);
    device.shared.playlist_read(
        &device.shared.writer(server(), "user", "alice"),
        &page(liked.clone(), &["1", "9"], &["1", "9"]),
    );
    device.shared.flush_db();
    let mut pages = Vec::new();
    for _ in 0..READ_ATTEMPTS {
        // Page one at revision 2, of three tracks; page two at revision
        // 3, after a reorder: 9 would never be read.
        let mut first = page(summary("liked", "Liked", 2), &["1", "2"], &["1", "2"]);
        first.playlist.builtin = Some(Builtin::Liked);
        first.total = 3;
        let mut second = page(summary("liked", "Liked", 3), &["3"], &["3"]);
        second.playlist.builtin = Some(Builtin::Liked);
        second.offset = 2;
        second.total = 3;
        pages.extend([first, second]);
    }
    let source = Edited {
        pages: std::sync::Mutex::new(pages),
    };
    read_with(
        &device.shared,
        &source,
        &device.shared.writer(server(), "user", "alice"),
        "liked",
    )
    .await
    .unwrap();
    let kept = device.on_db("user", |conn| {
        conn.query_row(
            "SELECT read_at, (SELECT count(*) FROM playlist_entries WHERE playlist = 'liked'),
             (SELECT liked FROM hearts WHERE track = 'plex:track:9')
             FROM playlists WHERE id = 'liked'",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            },
        )
        .unwrap()
    });
    // Still revision 1's two tracks, and 9 still liked.
    assert_eq!(kept, (1, 2, true));
}

/// Pages of one playlist, with the profile forgotten while the second is
/// read: as forget_profile running while the background reader waits on
/// the server.
struct ForgottenMidRead {
    shared: Arc<Shared>,
    asked: std::sync::Mutex<u32>,
}

impl Pages for ForgottenMidRead {
    async fn page(&self, _: &str, offset: u32, _: u32) -> Result<PlaylistPage, SdkError> {
        let asked = {
            let mut asked = self.asked.lock().unwrap();
            *asked += 1;
            *asked
        };
        if asked == 2 {
            self.shared
                .forget_playlists(&server().to_string(), Some("alice"))
                .await;
        }
        let mut page = page(summary("p1", "Road Trip", 2), &["1", "2"], &["1"]);
        page.total = 4;
        page.offset = offset;
        Ok(page)
    }

    async fn cover(&self, _: &ArtworkId) -> Result<Vec<u8>, SdkError> {
        Err(SdkError::NotFound("no cover".into()))
    }
}

/// One whole page of a playlist with a cover, with the profile forgotten
/// while the cover is fetched.
struct ForgottenMidCover {
    shared: Arc<Shared>,
}

impl Pages for ForgottenMidCover {
    async fn page(&self, _: &str, offset: u32, _: u32) -> Result<PlaylistPage, SdkError> {
        let mut p = summary("p1", "Road Trip", 2);
        p.artwork = Some("plex:art:1".parse::<ArtworkId>().unwrap());
        let mut page = page(p, &["1", "2"], &[]);
        page.offset = offset;
        Ok(page)
    }

    async fn cover(&self, _: &ArtworkId) -> Result<Vec<u8>, SdkError> {
        self.shared
            .forget_playlists(&server().to_string(), Some("alice"))
            .await;
        Ok(b"jpg".to_vec())
    }
}

/// Codex, PR #72: a cover fetched while its profile was forgotten was
/// still written, its folder made again; only the commit after it was
/// refused. Neither the folder nor the picture comes back.
#[tokio::test]
async fn a_cover_fetched_as_its_profile_is_forgotten_is_not_kept() {
    let device = device("forgotten-mid-cover");
    let writer = device.shared.writer(server(), "user", "alice");
    device.listed(vec![summary("p1", "Road Trip", 1)]);
    device.shared.flush_db();
    let source = ForgottenMidCover {
        shared: device.shared.clone(),
    };
    let read = read_with(&device.shared, &source, &writer, "p1").await;
    device.shared.flush_db();
    let dir = device.store.user_dir(&device.server(), "user");
    assert_eq!(
        (
            read.is_err(),
            dir.join("playlist-art").exists(),
            dir.join("offline.sqlite3").exists()
        ),
        (true, false, false)
    );
}

/// A profile forgotten while its playlist is read stays forgotten: what
/// the read brings back is dropped, its database is not made again, and
/// nothing of it is queued.
#[tokio::test]
async fn a_profile_forgotten_mid_read_is_not_written_again() {
    let device = device("forgotten-mid-read");
    let writer = device.shared.writer(server(), "user", "alice");
    device.listed(vec![summary("p1", "Road Trip", 1)]);
    device.shared.flush_db();
    let source = ForgottenMidRead {
        shared: device.shared.clone(),
        asked: std::sync::Mutex::new(0),
    };
    let read = read_with(&device.shared, &source, &writer, "p1").await;
    // A list read answered after the forget, as a screen's would be.
    device
        .shared
        .playlists_listed(&writer, &list(vec![summary("p1", "Road Trip", 2)]));
    device.shared.flush_db();
    let file = device
        .store
        .user_dir(&device.server(), "user")
        .join("offline.sqlite3");
    assert_eq!(
        (read.is_err(), file.exists(), device.queued()),
        (true, false, vec![])
    );
}

/// A playlist of four tracks, two a page, with a cover; the device moves
/// onto a mobile network (cellular not allowed) while page `held_at` is
/// asked for: before it is answered, or never answered at all.
struct HeldMidRead {
    shared: Arc<Shared>,
    held_at: u32,
    answer_held: bool,
    asked: std::sync::Mutex<u32>,
    covers: std::sync::Mutex<u32>,
}

impl Pages for HeldMidRead {
    async fn page(&self, _: &str, offset: u32, _: u32) -> Result<PlaylistPage, SdkError> {
        let asked = {
            let mut asked = self.asked.lock().unwrap();
            *asked += 1;
            *asked
        };
        if asked == self.held_at {
            *self.shared.network.lock() = super::super::NetworkKind::Cellular;
            self.shared.network_changed();
            if !self.answer_held {
                std::future::pending::<()>().await;
            }
        }
        let mut p = summary("p1", "Road Trip", 2);
        p.artwork = Some("plex:art:1".parse::<ArtworkId>().unwrap());
        let keys: &[&str] = if offset == 0 {
            &["1", "2"]
        } else {
            &["3", "4"]
        };
        let mut page = page(p, keys, &[]);
        page.total = 4;
        page.offset = offset;
        Ok(page)
    }

    async fn cover(&self, _: &ArtworkId) -> Result<Vec<u8>, SdkError> {
        *self.covers.lock().unwrap() += 1;
        Ok(b"jpg".to_vec())
    }
}

/// Read `p1` from a [`HeldMidRead`]: how many pages and covers were asked
/// for, what is kept of it (`read_at`, entries, staged), and the queue.
async fn read_held(
    test: &str,
    held_at: u32,
    answer_held: bool,
) -> (u32, u32, (Option<i64>, i64, i64), Vec<Job>) {
    let device = device(test);
    let writer = device.shared.writer(server(), "user", "alice");
    device.listed(vec![summary("p1", "Road Trip", 1)]);
    device.clear();
    let source = HeldMidRead {
        shared: device.shared.clone(),
        held_at,
        answer_held,
        asked: std::sync::Mutex::new(0),
        covers: std::sync::Mutex::new(0),
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        read_with(&device.shared, &source, &writer, "p1"),
    )
    .await
    .expect("a held read lets go")
    .unwrap();
    let kept = device.on_db("user", |conn| {
        conn.query_row(
            "SELECT read_at,
             (SELECT count(*) FROM playlist_entries WHERE playlist = 'p1'),
             (SELECT count(*) FROM playlist_staging WHERE playlist = 'p1')
             FROM playlists WHERE id = 'p1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    });
    let asked = *source.asked.lock().unwrap();
    let covers = *source.covers.lock().unwrap();
    (asked, covers, kept, device.queued())
}

/// Codex, PR #72: the reader looked at the network only as a job began,
/// so moving onto cellular (not allowed) mid-playlist went on reading its
/// pages and its cover. It stops before the next page: nothing more is
/// asked for or kept, what was staged goes, and the playlist waits in the
/// queue for a network that allows it.
#[tokio::test]
async fn a_read_stops_at_the_next_page_once_cellular_is_held() {
    assert_eq!(
        read_held("held-between-pages", 1, true).await,
        (1, 0, (None, 0, 0), vec![Job::Playlist("p1".into())])
    );
}

/// A page asked for as the device moves onto cellular (not allowed) is
/// let go of without its answer: the read stops as it would between
/// pages.
#[tokio::test]
async fn a_page_in_flight_when_cellular_is_held_is_let_go() {
    assert_eq!(
        read_held("held-in-flight", 2, false).await,
        (2, 0, (None, 0, 0), vec![Job::Playlist("p1".into())])
    );
}

/// Codex, PR #72: a background read staged revision 1 and waited on its
/// cover while a screen's read kept revision 2 whole; the commit then put
/// revision 1 back. An older snapshot never replaces a newer one: its
/// staging goes, and revision 2's summary and entries stay.
#[test]
fn an_older_staged_read_never_replaces_a_newer_whole_one() {
    let device = device("older-staged");
    device.listed(vec![summary("p1", "Road Trip", 1)]);
    let older = page(summary("p1", "Road Trip", 1), &["1", "2", "3"], &[]);
    let newer = page(summary("p1", "Road Trip Two", 2), &["4"], &[]);
    let kept = device.on_db("user", move |conn| {
        let tx = conn.transaction().unwrap();
        local_db::stage(&tx, "p1", 0, &older.entries).unwrap();
        tx.commit().unwrap();
        local_db::put_whole(conn, &newer.playlist, &newer.entries).unwrap();
        local_db::commit_staged(conn, &older.playlist, 3, None).unwrap();
        conn.query_row(
            "SELECT revision, read_at, json_extract(summary, '$.name'),
             (SELECT group_concat(item_id) FROM playlist_entries WHERE playlist = 'p1'),
             (SELECT count(*) FROM playlist_staging)
             FROM playlists WHERE id = 'p1'",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            },
        )
        .unwrap()
    });
    assert_eq!(
        kept,
        (
            2,
            2,
            "Road Trip Two".to_owned(),
            "plex:track:4".to_owned(),
            0
        )
    );
}

/// A list, or a partial page, answered after a later read of a playlist
/// leaves that read's summary and search row: a playlist's revision never
/// moves back, and it is not queued again for it.
#[test]
fn an_older_list_or_page_never_moves_a_playlist_back() {
    let device = device("older-list");
    let writer = device.shared.writer(server(), "user", "alice");
    device.listed(vec![summary("p1", "Road Trip", 1)]);
    device.shared.playlist_read(
        &writer,
        &page(summary("p1", "Road Trip Two", 2), &["4"], &[]),
    );
    device.clear();
    device.listed(vec![summary("p1", "Road Trip", 1)]);
    let mut partial = page(summary("p1", "Road Trip", 1), &["1"], &[]);
    partial.total = 3;
    device.shared.playlist_read(&writer, &partial);
    let kept = device.on_db("user", |conn| {
        conn.query_row(
            "SELECT revision, read_at, json_extract(summary, '$.name') FROM playlists
             WHERE id = 'p1'",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .unwrap()
    });
    // Found by the later read's name.
    let by_name = device.on_db("user", |conn| {
        local_db::find_playlists(conn, "\"two\"*").unwrap()
    });
    assert_eq!(
        (kept, by_name, device.queued()),
        (
            (2, 2, "Road Trip Two".to_owned()),
            vec!["p1".to_owned()],
            vec![]
        )
    );
}
