//! The downloads as an offline library shows them, worked out with no
//! server.

use super::*;
use crate::LocalWatch;
use crate::downloads::store::{Hold, Store};
use spoolframe_protocol::downloads::{DownloadFailure, DownloadSource, DownloadTracks};
use spoolframe_types::MediaTracks;

fn movie(id: &str, title: &str) -> Item {
    serde_json::from_value(serde_json::json!({
        "kind": "movie", "id": id, "title": title, "year": 2016,
        "release_date": null, "summary": null, "content_rating": null,
        "duration": 6_000_000, "artwork": [],
        "playback": { "position": null, "watched": false },
    }))
    .expect("a movie")
}

fn show(id: &str, title: &str) -> Item {
    serde_json::from_value(serde_json::json!({
        "kind": "show", "id": id, "title": title, "year": 2016,
        "summary": null, "content_rating": null, "season_count": 2,
        "genres": [], "artwork": [],
    }))
    .expect("a show")
}

/// Episode `episode` of season `season` of show `show`.
fn episode(show: &str, season: u32, episode: u32) -> Item {
    serde_json::from_value(serde_json::json!({
        "kind": "episode", "id": format!("plex:episode:{show}{season}{episode}"),
        "show_id": format!("plex:show:{show}"), "show_title": show,
        "season_id": format!("plex:season:{show}{season}"),
        "season": season, "episode": episode, "title": format!("{season}x{episode}"),
        "summary": null, "content_rating": null, "year": null, "air_date": null,
        "duration": 3_000_000, "artwork": [],
        "playback": { "position": null, "watched": false },
    }))
    .expect("an episode")
}

fn download(key: &str, item: Item, phase: DownloadPhase, created_at: i64) -> LocalDownload {
    LocalDownload {
        key: key.into(),
        server: "s".into(),
        user_id: "u".into(),
        username: "alice".into(),
        item,
        show: None,
        season: None,
        markers: Vec::new(),
        artwork: Vec::new(),
        tracks: MediaTracks::default(),
        source: DownloadSource::Original,
        chosen: DownloadTracks::All,
        file: None,
        phase,
        received: 0,
        size: None,
        expected_size: None,
        file_path: None,
        file_url: None,
        watch: LocalWatch::default(),
        note: None,
        created_at,
    }
}

fn done(key: &str, item: Item) -> LocalDownload {
    download(key, item, DownloadPhase::Done, 1)
}

/// Stopped at a minute in, at `at`.
fn begun(mut d: LocalDownload, at: i64) -> LocalDownload {
    d.watch = LocalWatch {
        position_ms: Some(60_000),
        watched: None,
        at,
        pending: true,
    };
    d
}

fn watched(mut d: LocalDownload) -> LocalDownload {
    d.watch.watched = Some(true);
    d
}

fn keys(downloads: &[LocalDownload]) -> Vec<&str> {
    downloads.iter().map(|d| d.key.as_str()).collect()
}

#[test]
fn the_library_groups_episodes_by_show_in_order() {
    let mut e2 = done("e2", episode("a", 1, 2));
    e2.show = Some(show("plex:show:a", "Andor"));
    let e1 = done("e1", episode("a", 1, 1));
    let s2 = done("s2", episode("a", 2, 1));
    let library = library(vec![s2, e2, e1]);
    let shows: Vec<_> = library.shows.iter().map(|s| keys(&s.episodes)).collect();
    assert_eq!(shows, [["e1", "e2", "s2"]]);
}

#[test]
fn the_library_sorts_shows_and_films_by_title() {
    let mut zed = done("z", episode("z", 1, 1));
    zed.show = Some(show("plex:show:z", "Zed"));
    let mut andor = done("a", episode("a", 1, 1));
    andor.show = Some(show("plex:show:a", "andor"));
    let library = library(vec![
        zed,
        andor,
        done("m2", movie("plex:movie:2", "Up")),
        done("m1", movie("plex:movie:1", "arrival")),
    ]);
    let shows: Vec<_> = library
        .shows
        .iter()
        .map(|s| s.show.id().to_string())
        .collect();
    assert_eq!(shows, ["plex:show:a", "plex:show:z"]);
    assert_eq!(keys(&library.films), ["m1", "m2"]);
}

#[test]
fn a_download_not_finished_is_in_progress_alone() {
    let failed = DownloadPhase::Failed {
        reason: "no".into(),
        cause: DownloadFailure::Network,
    };
    let library = library(vec![
        download("late", episode("a", 1, 1), DownloadPhase::Transferring, 9),
        download("early", movie("plex:movie:1", "Up"), failed, 2),
    ]);
    assert_eq!(
        (
            keys(&library.in_progress),
            library.shows.len(),
            library.films.len()
        ),
        (vec!["early", "late"], 0, 0)
    );
}

#[test]
fn continue_watching_is_the_latest_begun_first() {
    let going = continue_watching(vec![
        begun(done("old", movie("plex:movie:1", "Up")), 1),
        begun(done("new", movie("plex:movie:2", "Her")), 5),
    ]);
    assert_eq!(keys(&going), ["new", "old"]);
}

#[test]
fn continue_watching_leaves_out_the_watched_and_the_not_begun() {
    let going = continue_watching(vec![
        watched(begun(done("seen", movie("plex:movie:1", "Up")), 1)),
        done("fresh", movie("plex:movie:2", "Her")),
        begun(
            download(
                "moving",
                movie("plex:movie:3", "Ran"),
                DownloadPhase::Transferring,
                1,
            ),
            3,
        ),
    ]);
    assert_eq!(keys(&going), Vec::<&str>::new());
}

#[test]
fn continue_watching_has_one_episode_per_show() {
    let going = continue_watching(vec![
        begun(done("e1", episode("a", 1, 1)), 1),
        begun(done("e2", episode("a", 1, 2)), 4),
    ]);
    assert_eq!(keys(&going), ["e2"]);
}

#[test]
fn a_show_resumes_at_the_episode_begun() {
    let next = show_resume(
        vec![
            watched(done("e1", episode("a", 1, 1))),
            begun(done("e3", episode("a", 1, 3)), 2),
            done("e2", episode("a", 1, 2)),
        ],
        &"plex:show:a".parse().unwrap(),
    );
    assert_eq!(next.map(|d| d.key), Some("e3".into()));
}

#[test]
fn a_show_goes_on_after_the_last_watched() {
    let next = show_resume(
        vec![
            done("e1", episode("a", 1, 1)),
            watched(done("e2", episode("a", 1, 2))),
            done("s2", episode("a", 2, 1)),
        ],
        &"plex:show:a".parse().unwrap(),
    );
    assert_eq!(next.map(|d| d.key), Some("s2".into()));
}

#[test]
fn with_the_last_watched_a_show_goes_back_to_one_missed() {
    let next = show_resume(
        vec![
            done("e1", episode("a", 1, 1)),
            watched(done("e2", episode("a", 1, 2))),
        ],
        &"plex:show:a".parse().unwrap(),
    );
    assert_eq!(next.map(|d| d.key), Some("e1".into()));
}

#[test]
fn a_show_all_watched_has_nothing_to_resume() {
    let next = show_resume(
        vec![watched(done("e1", episode("a", 1, 1)))],
        &"plex:show:a".parse().unwrap(),
    );
    assert_eq!(next, None);
}

#[test]
fn the_finished_copy_of_an_item_wins_over_a_newer_one() {
    let id = "plex:movie:1";
    let copy = of_item(
        vec![
            download("old", movie(id, "Up"), DownloadPhase::Done, 1),
            download("new", movie(id, "Up"), DownloadPhase::Transferring, 9),
            download(
                "other",
                movie("plex:movie:2", "Her"),
                DownloadPhase::Done,
                5,
            ),
        ],
        "s",
        &id.parse().unwrap(),
    );
    assert_eq!(copy.map(|d| d.key), Some("old".into()));
}

/// A store with a finished film of `size` bytes, keyed `key`, held by
/// `holds`.
fn stored(shared: &Shared, key: &str, size: u64, holds: Vec<Hold>) {
    let mut m = Store::manifest(
        EntryKind::Film,
        key.into(),
        "s".into(),
        "u1".into(),
        "alice".into(),
        movie(&format!("plex:movie:{key}"), "Up"),
        MediaTracks::default(),
        DownloadSource::Original,
        DownloadTracks::All,
        LocalWatch::default(),
        1,
    );
    m.phase = DownloadPhase::Done;
    m.size = Some(size);
    m.holds = holds;
    shared.store().unwrap().write(&m).unwrap();
}

fn store(test: &str) -> std::sync::Arc<Shared> {
    let dir = std::env::temp_dir().join(format!("sf-sdk-library-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Shared::new(Some(dir))
}

#[test]
fn a_file_two_profiles_hold_is_counted_once_and_owned_by_neither() {
    let shared = store("shared-file");
    stored(
        &shared,
        "aa",
        100,
        vec![Hold::download("u1"), Hold::download("u2")],
    );
    stored(&shared, "bb", 10, vec![Hold::download("u1")]);
    let here = profiles_here(&shared);
    let tally: Vec<_> = here
        .profiles
        .iter()
        .map(|p| (p.user_id.as_str(), p.downloads, p.bytes, p.own_bytes))
        .collect();
    assert_eq!(
        (here.total_bytes, tally),
        (110, vec![("u1", 2, 110, 10), ("u2", 1, 100, 0)])
    );
}

#[test]
fn a_file_also_kept_from_listening_is_not_the_profiles_own() {
    let shared = store("listening");
    stored(
        &shared,
        "aa",
        100,
        vec![Hold::download("u1"), Hold::listening()],
    );
    let here = profiles_here(&shared);
    assert_eq!(here.profiles[0].own_bytes, 0);
}

#[test]
fn a_profile_is_named_by_a_download_it_fetched() {
    let shared = store("named");
    stored(&shared, "aa", 1, vec![Hold::download("u1")]);
    let here = profiles_here(&shared);
    assert_eq!(here.profiles[0].username, "alice");
}
