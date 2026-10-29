//! A download as an app shows it, worked out with no server.

use super::*;
use crate::LocalWatch;
use spoolframe_protocol::downloads::{DownloadFile, DownloadSource, FileTracks};
use spoolframe_types::{AudioTrack, Container, SubtitleTrack};

fn audio(language: &str) -> AudioTrack {
    AudioTrack {
        language: Some(language.into()),
        title: None,
        channels: None,
        default: false,
        role: Default::default(),
    }
}

fn subtitle(language: &str) -> SubtitleTrack {
    SubtitleTrack {
        language: Some(language.into()),
        title: None,
        forced: false,
        default: false,
        external: false,
        role: Default::default(),
        timing: None,
        download: None,
    }
}

/// A film with English, French and German audio and English and
/// Spanish subtitles, `phase`.
fn download(phase: DownloadPhase) -> LocalDownload {
    LocalDownload {
        key: "k1".into(),
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
        tracks: MediaTracks {
            audio: vec![audio("en"), audio("fr"), audio("de")],
            subtitles: vec![subtitle("en"), subtitle("es")],
            ..MediaTracks::default()
        },
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

fn failed(cause: DownloadFailure) -> DownloadPhase {
    DownloadPhase::Failed {
        reason: "it failed".into(),
        cause,
    }
}

fn languages(tracks: &MediaTracks) -> (Vec<String>, Vec<String>) {
    let of = |l: &Option<String>| l.clone().unwrap_or_default();
    (
        tracks.audio.iter().map(|t| of(&t.language)).collect(),
        tracks.subtitles.iter().map(|t| of(&t.language)).collect(),
    )
}

/// The file holds French audio and Spanish subtitles: the item's places
/// 1 and 1.
fn with_file(mut d: LocalDownload) -> LocalDownload {
    d.file = Some(DownloadFile {
        container: Container::Mp4,
        tracks: FileTracks {
            audio: vec![1],
            subtitles: vec![1],
        },
    });
    d
}

#[test]
fn a_watched_download_plays_from_the_start() {
    let mut d = download(DownloadPhase::Done);
    d.watch = LocalWatch {
        position_ms: Some(60_000),
        watched: Some(true),
        at: 5,
        pending: true,
    };
    assert_eq!((d.watched_here(), d.resume_point()), (true, None));
}

/// A place of 0 is the start, as an item's own resume point has it.
#[test]
fn a_download_stopped_at_its_start_plays_from_the_start() {
    let mut d = download(DownloadPhase::Done);
    d.watch = LocalWatch {
        position_ms: Some(0),
        watched: None,
        at: 5,
        pending: true,
    };
    assert_eq!(d.resume_point(), None);
}

#[test]
fn a_place_the_server_has_not_heard_wins_over_the_asked_one() {
    let mut d = download(DownloadPhase::Done);
    d.watch = LocalWatch {
        position_ms: Some(60_000),
        watched: None,
        at: 5,
        pending: true,
    };
    let asked = Some(Duration::from_secs(10));
    assert_eq!(d.resume_from(asked), Some(Duration::from_secs(60)));
    d.watch.pending = false;
    assert_eq!(d.resume_from(asked), Some(Duration::from_secs(10)));
    assert_eq!(d.resume_from(None), None);
}

#[test]
fn a_failure_is_retried_by_its_cause() {
    let retry = |cause| download(failed(cause)).can_retry();
    assert!(retry(DownloadFailure::NoSpace { needed_bytes: 1 }));
    assert!(retry(DownloadFailure::Network));
    assert!(retry(DownloadFailure::ServerStoppedConverting));
    assert!(!retry(DownloadFailure::SourceMissing));
    assert!(!retry(DownloadFailure::NoLongerAvailable));
}

#[test]
fn another_failure_is_retried_only_once_its_size_was_known() {
    let mut d = download(failed(DownloadFailure::Other));
    assert!(!d.can_retry());
    d.size = Some(10);
    assert!(d.can_retry());
}

#[test]
fn the_actions_follow_the_phase() {
    use DownloadAction::*;
    assert_eq!(
        download(DownloadPhase::Transferring).actions(),
        [Pause, Cancel]
    );
    assert_eq!(download(DownloadPhase::Done).actions(), [Play, Delete]);
    assert_eq!(
        download(failed(DownloadFailure::Network)).actions(),
        [Retry, Dismiss]
    );
    assert_eq!(
        download(failed(DownloadFailure::SourceMissing)).actions(),
        [Dismiss]
    );
}

#[test]
fn progress_is_the_servers_then_the_bytes_here() {
    let converting = download(DownloadPhase::Preparing {
        server: DownloadState::Converting {
            done_percent: Some(40),
            eta_secs: None,
        },
    });
    assert_eq!(converting.progress(), Some(0.4));
    let mut moving = download(DownloadPhase::Transferring);
    moving.received = 250;
    moving.expected_size = Some(1000);
    assert_eq!(moving.progress(), Some(0.25));
    moving.size = Some(500);
    assert_eq!(moving.progress(), Some(0.5));
    assert_eq!(download(DownloadPhase::Done).progress(), None);
}

#[test]
fn the_file_tracks_are_what_the_file_carries_in_its_order() {
    let d = with_file(download(DownloadPhase::Done));
    assert_eq!(
        languages(&d.file_tracks()),
        (vec!["fr".to_owned()], vec!["es".to_owned()])
    );
}

/// In the item's order: the server writes the file's tracks so.
#[test]
fn before_the_server_says_the_file_is_what_was_asked() {
    let mut d = download(DownloadPhase::Transferring);
    d.chosen = DownloadTracks::Chosen {
        audio: vec![2, 0],
        subtitles: vec![],
    };
    assert_eq!(
        languages(&d.file_tracks()),
        (vec!["en".to_owned(), "de".to_owned()], vec![])
    );
}

#[test]
fn a_choice_of_the_items_tracks_is_the_same_tracks_of_the_file() {
    let d = with_file(download(DownloadPhase::Done));
    let picked = d.file_selection(Selection {
        audio: Some(1),
        subtitle: Some(1),
    });
    assert_eq!(
        picked,
        Selection {
            audio: Some(0),
            subtitle: Some(0)
        }
    );
}

#[test]
fn a_track_the_file_lacks_is_the_default_audio_or_no_subtitles() {
    let d = with_file(download(DownloadPhase::Done));
    let picked = d.file_selection(Selection {
        audio: Some(2),
        subtitle: Some(0),
    });
    assert_eq!(
        picked,
        Selection {
            audio: None,
            subtitle: None
        }
    );
}

#[test]
fn with_nothing_asked_the_files_first_audio_plays_without_subtitles() {
    let d = with_file(download(DownloadPhase::Done));
    assert_eq!(
        d.pick_tracks(None),
        Selection {
            audio: Some(0),
            subtitle: None
        }
    );
}

#[test]
fn the_list_follows_the_events() {
    let mut list = DownloadList::new(vec![download(DownloadPhase::Transferring)]);
    let mut done = download(DownloadPhase::Done);
    assert_eq!(
        list.apply(&DownloadEvent::Changed(done.clone())),
        Applied::Updated
    );
    done.key = "k2".into();
    assert_eq!(list.apply(&DownloadEvent::Changed(done)), Applied::Added);
    let removed = DownloadEvent::Removed { key: "k1".into() };
    assert_eq!(list.apply(&removed), Applied::Removed);
    assert_eq!(list.apply(&removed), Applied::Nothing);
    let keys: Vec<_> = list.downloads.iter().map(|d| d.key.as_str()).collect();
    assert_eq!(keys, ["k2"]);
}
