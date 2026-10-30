//! The app's side of playback: a playback the caller never received
//! closes its token (and URL), a `SpoolPlayback` reads its bytes, follows
//! its new plans and reports where the player is, and a served
//! playback's URL has the shape the player depends on. The bridge's own
//! tests are in spoolframe-hls-bridge.

use super::*;
use sdk::{Chunk, PlaybackBackend, PlaybackChange, PlaybackHandle, SdkError};
use spoolframe_client_sdk::types::Container;

/// A direct play's server.
#[derive(Default)]
struct Backend {
    /// The subtitle file it has, once read as text; `None`: still reading.
    subtitle: Option<sdk::Subtitle>,
    /// The positions reported to it.
    progress: Mutex<Vec<Duration>>,
    /// A subtitle change encodes again (a conversion's); else it plays
    /// on as it is (a direct play's).
    reencodes: bool,
    /// The next read finds the playback ended on the server.
    expire: std::sync::atomic::AtomicBool,
    /// Playbacks started: each under a handle of its own.
    starts: std::sync::atomic::AtomicU32,
}

#[async_trait::async_trait]
impl PlaybackBackend for Backend {
    async fn start(
        &self,
        _: &MediaId,
        _: Option<Selection>,
        _: sdk::PlaybackOptions,
    ) -> Result<sdk::Playback, SdkError> {
        let n = self
            .starts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(sdk::Playback {
            handle: PlaybackHandle(format!("started-{n}")),
            ..snapshot()
        })
    }
    async fn read(&self, _: &PlaybackHandle, _: u64, length: u64) -> Result<Vec<u8>, SdkError> {
        if self.expire.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err(SdkError::PlaybackExpired);
        }
        Ok(vec![1; length as usize])
    }
    async fn read_chunk(
        &self,
        _: &PlaybackHandle,
        epoch: u32,
        _: u64,
        _: u64,
    ) -> Result<Chunk, SdkError> {
        Ok(Chunk {
            bytes: vec![epoch as u8],
            size: 2,
            complete: true,
            epoch,
        })
    }
    async fn read_segment(
        &self,
        _: &PlaybackHandle,
        epoch: u32,
        _: u32,
        _: u32,
        _: u64,
        _: u64,
    ) -> Result<Chunk, SdkError> {
        Ok(Chunk {
            bytes: vec![epoch as u8],
            size: 2,
            complete: true,
            epoch,
        })
    }
    async fn seek(&self, _: &PlaybackHandle, _: Duration) -> Result<u32, SdkError> {
        Ok(1)
    }
    async fn subtitle(
        &self,
        _: &PlaybackHandle,
        _: Option<u32>,
        _: &sdk::Replan,
    ) -> Result<PlaybackChange, SdkError> {
        Ok(PlaybackChange {
            plan: snapshot().plan,
            size: 2,
            epoch: u32::from(self.reencodes),
        })
    }
    /// Planned again: the file in another container.
    async fn change_quality(
        &self,
        _: &PlaybackHandle,
        _: sdk::PlaybackQuality,
        _: &sdk::Replan,
    ) -> Result<PlaybackChange, SdkError> {
        Ok(PlaybackChange {
            plan: replanned().plan,
            size: 2,
            epoch: 0,
        })
    }
    async fn progress(&self, _: &PlaybackHandle, position: Duration) -> Result<(), SdkError> {
        self.progress.lock().unwrap().push(position);
        Ok(())
    }
    async fn stop(&self, _: &PlaybackHandle) -> Result<(), SdkError> {
        Ok(())
    }
    fn link_meter(&self) -> Option<std::sync::Arc<spoolframe_client_sdk::LinkMeter>> {
        None
    }
    fn shares(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<spoolframe_client_sdk::PlaybackShare>> {
        None
    }
    async fn set_track_override(
        &self,
        _: &spoolframe_client_sdk::types::MediaId,
        _: spoolframe_client_sdk::Selection,
    ) -> Result<(), spoolframe_client_sdk::SdkError> {
        Err(spoolframe_client_sdk::SdkError::InvalidInput(
            "no track choices here".into(),
        ))
    }
    async fn read_subtitle(
        &self,
        _: &spoolframe_client_sdk::PlaybackHandle,
    ) -> Result<spoolframe_client_sdk::Subtitle, spoolframe_client_sdk::SdkError> {
        self.subtitle
            .clone()
            .ok_or(SdkError::SubtitleNotReady { done_percent: 40 })
    }
    fn subtitle_timings(
        &self,
    ) -> Option<(
        String,
        tokio::sync::broadcast::Receiver<spoolframe_client_sdk::SubtitleTimingEvent>,
    )> {
        None
    }
    fn route(&self) -> Option<spoolframe_client_sdk::RemoteMode> {
        None
    }
}

fn snapshot() -> sdk::Playback {
    sdk::Playback {
        handle: PlaybackHandle("test-playback".into()),
        plan: PlaybackPlan::Direct {
            container: Container::Mp4,
            video: None,
            audio: None,
            subtitle: None,
            source_codec: None,
        },
        size: 2,
        chosen: None,
        epoch: 0,
        markers: vec![],
        duration: None,
    }
}

/// The playback as the SDK planned it again behind the player.
fn replanned() -> sdk::Playback {
    let mut playback = snapshot();
    if let PlaybackPlan::Direct { container, .. } = &mut playback.plan {
        *container = Container::Matroska;
    }
    playback
}

/// Have the SDK plan `playback` again behind the player, as it does on a
/// slow link.
async fn plan_again_behind(playback: &SpoolPlayback) {
    let managed = playback.registered().unwrap().managed.clone();
    managed
        .change_quality(sdk::PlaybackQuality::Auto, Duration::ZERO)
        .await
        .unwrap();
}

fn managed() -> sdk::ManagedPlayback {
    adopted(Arc::new(Backend::default()), snapshot())
}

fn adopted(backend: Arc<Backend>, playback: sdk::Playback) -> sdk::ManagedPlayback {
    sdk::ManagedPlayback::adopt(
        backend,
        "plex:movie:1".parse().unwrap(),
        None,
        sdk::PlaybackOptions::default(),
        playback,
    )
}

/// A playback of `backend`'s, as the app holds it.
fn playing(backend: Arc<Backend>, playback: sdk::Playback) -> Arc<SpoolPlayback> {
    let opened = open_under_token(
        adopted(backend.clone(), playback),
        None,
        Default::default(),
        None,
    );
    SpoolPlayback::new(opened)
}

/// A direct play with an external subtitle: one that comes as a file.
fn with_external_subtitle() -> sdk::Playback {
    let mut playback = snapshot();
    if let PlaybackPlan::Direct { subtitle, .. } = &mut playback.plan {
        *subtitle = Some(sdk::PlannedSubtitle {
            index: 3,
            format: SubtitleFormat::Srt,
            delivery: sdk::SubtitleDelivery::External,
        });
    }
    playback
}

#[tokio::test]
async fn an_abandoned_completed_result_closes_its_token() {
    let opened = open_under_token(managed(), None, Default::default(), None);
    let token = opened.0.as_ref().unwrap().playback.token.clone();
    assert!(registered(&token).is_some());
    drop(opened);
    assert!(registered(&token).is_none());
}

#[tokio::test]
async fn a_dropped_playback_closes_its_token() {
    let playback = playing(Arc::default(), snapshot());
    let token = playback.token();
    drop(playback);
    assert!(registered(&token).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_opened_playback_has_no_url_and_reads_its_file() {
    let playback = playing(Arc::default(), snapshot());
    let reading = playback.clone();
    let bytes = tokio::task::spawn_blocking(move || reading.read(0, 2))
        .await
        .unwrap();
    assert_eq!((playback.info().url, bytes), (None, Ok(vec![1, 1])));
}

#[tokio::test]
async fn an_opened_file_reads_as_its_size_and_its_container_s_type() {
    let playback = playing(Arc::default(), snapshot());
    assert_eq!(
        (playback.size(), playback.content_type()),
        (Some(2), "video/mp4".to_owned())
    );
}

#[tokio::test]
async fn a_finished_playback_reports_where_it_ended_and_reads_no_more() {
    let backend = Arc::new(Backend::default());
    let playback = playing(backend.clone(), snapshot());
    playback.finish(Duration::from_secs(42)).await.unwrap();
    assert_eq!(
        (
            backend.progress.lock().unwrap().last().copied(),
            playback.report_position(Duration::ZERO)
        ),
        (
            Some(Duration::from_secs(42)),
            Err(SpoolError::PlaybackExpired)
        )
    );
}

#[tokio::test]
async fn a_flush_sends_the_position_reported_last() {
    let backend = Arc::new(Backend::default());
    let playback = playing(backend.clone(), snapshot());
    for seconds in [1, 2, 3] {
        playback
            .report_position(Duration::from_secs(seconds))
            .unwrap();
    }
    playback.flush().await.unwrap();
    assert_eq!(
        backend.progress.lock().unwrap().last().copied(),
        Some(Duration::from_secs(3))
    );
}

/// While the player's seek is in flight it still says where it was: the
/// progress saved is the seek's target, never the place it left, until
/// the player says the seek is done.
#[tokio::test]
async fn positions_during_a_seek_report_its_target() {
    let backend = Arc::new(Backend::default());
    let playback = playing(backend.clone(), snapshot());
    playback.report_position(Duration::from_secs(65)).unwrap();
    playback.seek_started(Duration::from_secs(300));
    playback.report_position(Duration::from_secs(65)).unwrap();
    playback.flush().await.unwrap();
    let during = backend.progress.lock().unwrap().last().copied();
    playback.seek_done();
    playback.report_position(Duration::from_secs(296)).unwrap();
    playback.flush().await.unwrap();
    let after = backend.progress.lock().unwrap().last().copied();
    assert_eq!(
        (during, after),
        (
            Some(Duration::from_secs(300)),
            Some(Duration::from_secs(296))
        )
    );
}

#[tokio::test]
async fn a_subtitle_change_moves_the_player_to_the_new_encode() {
    let backend = Arc::new(Backend {
        reencodes: true,
        ..Default::default()
    });
    let playback = playing(backend, snapshot());
    let before = playback.token();
    let changed = playback
        .clone()
        .change_subtitle(Some(3), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(
        (
            changed.is_some(),
            registered(&before).is_none(),
            registered(&playback.token()).map(|p| *p.subtitle.lock().unwrap())
        ),
        (true, true, Some(Some(3)))
    );
}

/// A direct play's subtitle change keeps its plan and encode, so the
/// player stays; a plan the SDK makes after still has that subtitle.
#[tokio::test(flavor = "multi_thread")]
async fn a_subtitle_kept_without_a_new_plan_carries_into_the_next_plan() {
    let playback = playing(Arc::default(), snapshot());
    let changed = playback
        .clone()
        .change_subtitle(Some(3), Duration::from_secs(10))
        .await
        .unwrap();
    plan_again_behind(&playback).await;
    let followed = playback.clone().follow_replan().await.unwrap();
    assert_eq!(
        (
            changed.is_none(),
            followed.is_some(),
            registered(&playback.token()).map(|p| *p.subtitle.lock().unwrap())
        ),
        (true, true, Some(Some(3)))
    );
}

/// Two loader threads that both find the plan stale, while a move is
/// under way: both are told `Replanned`, never `PlaybackExpired`, and
/// one new plan is made.
#[tokio::test(flavor = "multi_thread")]
async fn two_reads_that_find_a_new_plan_both_say_replanned() {
    let playback = playing(Arc::default(), snapshot());
    let before = playback.token();
    plan_again_behind(&playback).await;
    let moving = playback.moving.lock().await;
    let reads: Vec<_> = (0..2)
        .map(|_| {
            let reading = playback.clone();
            tokio::task::spawn_blocking(move || reading.read(0, 2))
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(moving);
    let mut answers = Vec::new();
    for read in reads {
        answers.push(read.await.unwrap());
    }
    let current = registered(&playback.token()).unwrap();
    let open = PLAYBACKS
        .lock()
        .unwrap()
        .values()
        .filter(|p| Arc::ptr_eq(&p.notes, &current.notes))
        .count();
    assert_eq!(
        (
            answers,
            registered(&before).is_none(),
            playback.token() != before,
            open
        ),
        (
            vec![Err(SpoolError::Replanned), Err(SpoolError::Replanned)],
            true,
            true,
            1
        )
    );
}

/// While the player's new plan is being served (here held on the
/// bridge's listener), its other calls go on against the plan it is on:
/// none of them finds it gone.
#[cfg(feature = "hls-bridge")]
#[tokio::test(flavor = "multi_thread")]
async fn a_call_while_the_new_plan_is_served_finds_the_playback_open() {
    let backend = Arc::new(Backend::default());
    let opened = open_under_token(
        adopted(backend.clone(), snapshot()),
        None,
        Default::default(),
        None,
    );
    let opened = run(crate::bridge::serve_opened(opened)).await.unwrap();
    let playback = SpoolPlayback::new(opened);
    plan_again_behind(&playback).await;
    let listener = crate::bridge::LISTENER.lock().await;
    let following = tokio::spawn(playback.clone().follow_replan());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let during = (
        playback.report_position(Duration::from_secs(5)),
        playback.expired(),
        playback.size(),
    );
    drop(listener);
    let followed = following.await.unwrap().unwrap();
    assert_eq!(
        (during, followed.is_some()),
        ((Ok(()), false, Some(2)), true)
    );
}

#[tokio::test]
async fn a_subtitle_file_is_read_whole() {
    let backend = Arc::new(Backend {
        subtitle: Some(sdk::Subtitle {
            format: SubtitleFormat::Srt,
            bytes: b"1\n00:00:01,000 --> 00:00:02,000\nHi\n".to_vec(),
        }),
        ..Default::default()
    });
    let playback = playing(backend, with_external_subtitle());
    let subtitle = playback.read_subtitle().await.unwrap();
    assert_eq!(subtitle.format, SubtitleFormat::Srt);
}

#[tokio::test]
async fn a_subtitle_still_read_as_text_is_not_ready_after_the_wait() {
    let managed = adopted(Arc::default(), with_external_subtitle());
    let read = managed
        .subtitle_file_within(Duration::from_millis(50))
        .await;
    assert_eq!(read, Err(SdkError::SubtitleNotReady { done_percent: 40 }));
}

#[tokio::test]
async fn a_reopened_playback_keeps_its_subtitle_menu() {
    let opened = open_under_token(managed(), None, Default::default(), None).take();
    let entry = SubtitleEntry {
        index: 2,
        name: "English".into(),
        language: Some("en".into()),
        forced: false,
    };
    let stream = registered(&opened.playback.token)
        .unwrap()
        .stream
        .clone()
        .unwrap();
    stream.set_menu(vec![entry.clone().into()]);
    let old = unregister(&opened.playback.token).unwrap();
    let new = reopen(&old, None, false).await.unwrap().take();
    let menu = registered(&new.playback.token)
        .unwrap()
        .stream
        .as_ref()
        .unwrap()
        .menu();
    assert_eq!(menu, Some(vec![MenuEntry::from(entry)]));
    unregister(&new.playback.token);
}

#[tokio::test]
async fn a_playback_the_bridge_does_not_serve_is_open_under_its_token() {
    let mut playback = snapshot();
    playback.plan = PlaybackPlan::Converted {
        delivery: Delivery::Progressive,
        container: Container::Matroska,
        video: None,
        audio: None,
        subtitle: None,
        ladder: Vec::new(),
        reasons: Vec::new(),
        source_codec: None,
    };
    let managed = adopted(Arc::default(), playback);
    let opened = open_under_token(managed, None, Default::default(), None).take();
    let stream = registered(&opened.playback.token).map(|p| p.stream.is_some());
    assert_eq!(stream, Some(false));
    unregister(&opened.playback.token);
}

/// A reader made for one plan reads the file as the SDK plans it now:
/// bytes that come back after a new plan are refused as `Replanned`, not
/// handed out at the old plan's offsets.
#[tokio::test(flavor = "multi_thread")]
async fn a_reader_s_bytes_from_after_a_new_plan_are_refused() {
    let playback = playing(Arc::default(), snapshot());
    let reader = crate::reader::reader(playback.registered().unwrap(), None).unwrap();
    plan_again_behind(&playback).await;
    let read = tokio::task::spawn_blocking(move || reader.read(0, 2))
        .await
        .unwrap();
    assert_eq!(read, Err(SpoolError::Replanned));
}

/// The server ended the playback and the SDK started it again, same
/// plan, new handle: the player moves there, where the viewer was, and
/// reads on from a new reader.
#[tokio::test(flavor = "multi_thread")]
async fn a_playback_started_again_moves_the_player() {
    let backend = Arc::new(Backend::default());
    let playback = playing(backend.clone(), snapshot());
    let before = playback.token();
    backend
        .expire
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let reading = playback.clone();
    let (first, again) = tokio::task::spawn_blocking(move || {
        let first = reading.clone().read(0, 2);
        (first, reading.read(0, 2))
    })
    .await
    .unwrap();
    assert_eq!(
        (first, again, playback.token() != before),
        (Err(SpoolError::Replanned), Ok(vec![1, 1]), true)
    );
}

/// A stopped playback keeps no reader: a read after says it is over.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_playback_reads_nothing() {
    let playback = playing(Arc::default(), snapshot());
    playback.stop().await;
    let reading = playback.clone();
    let read = tokio::task::spawn_blocking(move || reading.read(0, 2))
        .await
        .unwrap();
    assert_eq!(read, Err(SpoolError::PlaybackExpired));
}

#[cfg(feature = "hls-bridge")]
#[tokio::test]
async fn a_direct_playback_is_served_as_its_file_on_loopback() {
    let opened = open_under_token(managed(), None, Default::default(), None);
    let opened = run(crate::bridge::serve_opened(opened))
        .await
        .map(RegisteredPlayback::take)
        .unwrap();
    let url = opened.start.url.clone().unwrap();
    assert!(
        !opened.start.hls
            && url.starts_with("http://127.0.0.1:")
            && url.ends_with(&format!("/{}/file", opened.playback.token)),
        "{url}"
    );
    unregister(&opened.playback.token);
}
