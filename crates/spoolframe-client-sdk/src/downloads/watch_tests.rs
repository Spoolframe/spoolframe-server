//! A download's watch here: each change is merged with what was there,
//! and a position at the end marks it watched.

use super::*;
use spoolframe_types::MarkerKind;

const USER: &str = "user";

/// A 100-minute film, with its credits from minute 95 when `credits`.
fn film(credits: bool) -> (Item, Vec<Marker>) {
    let item = serde_json::from_value(serde_json::json!({
        "kind": "movie", "id": "plex:movie:1", "title": "Arrival", "year": 2016,
        "release_date": null, "summary": null, "content_rating": null,
        "duration": 6_000_000, "artwork": [],
        "playback": { "position": null, "watched": false },
    }))
    .expect("a movie");
    let markers = if credits {
        vec![Marker {
            kind: MarkerKind::Credits,
            start_ms: 5_700_000,
            end_ms: 6_000_000,
            to_end: true,
        }]
    } else {
        Vec::new()
    };
    (item, markers)
}

/// A shared download store with the film downloaded by [`USER`].
fn downloaded(test: &str, credits: bool) -> (Arc<Shared>, String) {
    let dir = std::env::temp_dir().join(format!("sf-sdk-watch-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let shared = Shared::new(Some(dir));
    let (item, markers) = film(credits);
    let mut m = Store::manifest(
        EntryKind::Film,
        "f11e".into(),
        "server".into(),
        USER.into(),
        "alice".into(),
        item,
        MediaTracks::default(),
        DownloadSource::Original,
        DownloadTracks::All,
        LocalWatch::default(),
        1,
    );
    m.markers = markers;
    m.phase = DownloadPhase::Done;
    shared.store().unwrap().write(&m).unwrap();
    (shared, m.key)
}

async fn record(shared: &Shared, key: &str, change: WatchChange) -> LocalWatch {
    shared
        .record_watch(key, USER, change)
        .await
        .unwrap()
        .watch_of(USER)
}

const MINUTE: u64 = 60_000;

/// sf-ios: a film marked watched lost its mark as soon as the player
/// reported where it was, because each write replaced the whole watch.
#[tokio::test]
async fn a_position_after_marking_watched_keeps_the_mark() {
    let (shared, key) = downloaded("keeps-mark", false);
    record(&shared, &key, WatchChange::Watched(true)).await;
    let watch = record(&shared, &key, WatchChange::Position(10 * MINUTE)).await;
    assert_eq!(
        (watch.watched, watch.position_ms),
        (Some(true), Some(10 * MINUTE))
    );
}

#[tokio::test]
async fn marking_watched_keeps_where_it_got_to() {
    let (shared, key) = downloaded("keeps-place", false);
    record(&shared, &key, WatchChange::Position(10 * MINUTE)).await;
    let watch = record(&shared, &key, WatchChange::Watched(true)).await;
    assert_eq!(watch.position_ms, Some(10 * MINUTE));
}

#[tokio::test]
async fn unwatched_starts_it_over() {
    let (shared, key) = downloaded("over", false);
    record(&shared, &key, WatchChange::Position(10 * MINUTE)).await;
    let watch = record(&shared, &key, WatchChange::Watched(false)).await;
    assert_eq!((watch.watched, watch.position_ms), (Some(false), None));
}

#[tokio::test]
async fn a_position_in_the_credits_marks_it_watched() {
    let (shared, key) = downloaded("credits", true);
    let watch = record(&shared, &key, WatchChange::Position(96 * MINUTE)).await;
    assert_eq!(watch.watched, Some(true));
}

#[tokio::test]
async fn without_credits_the_last_30_seconds_mark_it_watched() {
    let (shared, key) = downloaded("near-end", false);
    let before = record(&shared, &key, WatchChange::Position(96 * MINUTE)).await;
    let at_end = record(&shared, &key, WatchChange::Position(100 * MINUTE - 20_000)).await;
    assert_eq!((before.watched, at_end.watched), (None, Some(true)));
}

#[tokio::test]
async fn a_position_mid_way_is_not_watched() {
    let (shared, key) = downloaded("mid", true);
    let watch = record(&shared, &key, WatchChange::Position(40 * MINUTE)).await;
    assert_eq!((watch.watched, watch.pending), (None, true));
}
