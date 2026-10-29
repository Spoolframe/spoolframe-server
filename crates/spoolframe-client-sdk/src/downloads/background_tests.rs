//! A background run's progress, and what a background refresh finds to
//! do, worked out with no server.

use super::*;
use crate::LocalWatch;
use crate::downloads::store::{EntryKind, Store};
use spoolframe_protocol::downloads::{DownloadSource, DownloadTracks};
use spoolframe_types::MediaTracks;

fn download(key: &str, phase: DownloadPhase) -> LocalDownload {
    LocalDownload {
        key: key.into(),
        server: "s".into(),
        user_id: "u".into(),
        username: "alice".into(),
        item: serde_json::from_value(serde_json::json!({
            "kind": "movie", "id": "plex:movie:1", "title": "Arrival", "year": 2016,
            "release_date": null, "summary": null, "content_rating": null,
            "duration": 6_000_000, "artwork": [],
            "playback": { "position": null, "watched": false },
        }))
        .unwrap(),
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
        created_at: 1,
    }
}

fn converting(percent: u8) -> DownloadPhase {
    DownloadPhase::Preparing {
        server: DownloadState::Converting {
            done_percent: Some(percent),
            eta_secs: None,
        },
    }
}

#[test]
fn a_conversion_fills_the_first_half() {
    let progress = RunTally::new().progress(&[download("a", converting(40))]);
    assert_eq!((progress.completed, progress.total), (200, 1000));
}

#[test]
fn a_transfer_fills_the_second_half() {
    let mut d = download("a", DownloadPhase::Transferring);
    d.received = 250;
    d.size = Some(1000);
    let progress = RunTally::new().progress(&[d]);
    assert_eq!(progress.completed, 625);
}

#[test]
fn a_download_finished_and_gone_stays_counted() {
    let mut tally = RunTally::new();
    tally.progress(&[
        download("a", converting(0)),
        download("b", DownloadPhase::Transferring),
    ]);
    let progress = tally.progress(&[download("b", DownloadPhase::Done)]);
    assert_eq!(
        (progress.completed, progress.total, progress.downloads),
        (2000, 2000, 2)
    );
}

#[test]
fn the_download_named_is_the_one_moving() {
    let progress = RunTally::new().progress(&[
        download(
            "waiting",
            DownloadPhase::Preparing {
                server: DownloadState::Waiting,
            },
        ),
        download("moving", DownloadPhase::Transferring),
    ]);
    assert_eq!(progress.moving.map(|d| d.key), Some("moving".into()));
}

fn store(test: &str) -> std::sync::Arc<Shared> {
    let dir = std::env::temp_dir().join(format!("sf-sdk-background-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Shared::new(Some(dir))
}

/// A finished film on server "s", with `user`'s watch `pending` or not.
fn stored(shared: &Shared, user: &str, pending: bool) {
    let mut m = Store::manifest(
        EntryKind::Film,
        "aa".into(),
        "s".into(),
        user.into(),
        "alice".into(),
        download("aa", DownloadPhase::Done).item,
        MediaTracks::default(),
        DownloadSource::Original,
        DownloadTracks::All,
        LocalWatch {
            pending,
            ..LocalWatch::default()
        },
        1,
    );
    m.phase = DownloadPhase::Done;
    shared.store().unwrap().write(&m).unwrap();
}

#[test]
fn a_watch_not_sent_is_to_send() {
    let shared = store("pending");
    stored(&shared, "u1", true);
    assert_eq!(to_send(&shared), [("s".to_owned(), "u1".to_owned())]);
}

#[test]
fn a_watch_sent_leaves_nothing_to_send() {
    let shared = store("sent");
    stored(&shared, "u1", false);
    assert_eq!(to_send(&shared), Vec::new());
}
