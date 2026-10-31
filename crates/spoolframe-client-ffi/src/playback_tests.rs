//! The app's side of playback: a playback the caller never received
//! closes its token (and URL), a `SpoolPlayback` reads its bytes, follows
//! its new plans and reports where the player is, and a served
//! playback's URL has the shape the player depends on. The bridge's own
//! tests are in spoolframe-hls-bridge.

use super::*;
use sdk::{Chunk, PlaybackBackend, PlaybackChange, PlaybackHandle, SdkError};
use spoolframe_client_sdk::types::Container;

const MIB: u64 = 1024 * 1024;

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
    /// Where it was asked to seek: each a new encode, its epoch the count.
    seeks: Mutex<Vec<Duration>>,
    /// Seeks wait on this: the server is slow to answer.
    seek_held: tokio::sync::Mutex<()>,
    /// Seeks are refused.
    seek_refused: std::sync::atomic::AtomicBool,
    /// A conversion's output, this long so far, in 2 MiB chunks of its
    /// epoch; `None`: two bytes.
    output: Option<u64>,
    /// The first encode's reads find it replaced: the server has the seek
    /// and the SDK hasn't its answer.
    replaced: std::sync::atomic::AtomicBool,
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
        offset: u64,
        _: u64,
    ) -> Result<Chunk, SdkError> {
        if epoch == 0 && self.replaced.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(SdkError::PlaybackRestarted { epoch: 1 });
        }
        if let Some(output) = self.output {
            let got = output.saturating_sub(offset).min(2 * MIB);
            if got == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            return Ok(Chunk {
                bytes: vec![epoch as u8; got as usize],
                size: offset + got,
                complete: false,
                epoch,
            });
        }
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
    async fn seek(&self, _: &PlaybackHandle, at: Duration) -> Result<u32, SdkError> {
        let _held = self.seek_held.lock().await;
        if self.seek_refused.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(SdkError::Busy);
        }
        let mut seeks = self.seeks.lock().unwrap();
        seeks.push(at);
        Ok(seeks.len() as u32)
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

/// A conversion, read as `delivery` gives it.
fn converted(delivery: Delivery) -> sdk::Playback {
    sdk::Playback {
        plan: PlaybackPlan::Converted {
            delivery,
            container: Container::Matroska,
            video: None,
            audio: None,
            subtitle: None,
            ladder: Vec::new(),
            reasons: Vec::new(),
            source_codec: None,
        },
        ..snapshot()
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
/// A movie with an intro from 60 s to 90 s.
fn with_an_intro() -> sdk::Playback {
    sdk::Playback {
        markers: vec![Marker {
            kind: sdk::types::MarkerKind::Intro,
            start: Duration::from_secs(60),
            end: Duration::from_secs(90),
            to_end: false,
        }],
        ..snapshot()
    }
}

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

/// A scrubber goes by the seek's target while the player still says where
/// it was, and by the player once the seek is done.
#[tokio::test]
async fn the_position_is_a_seek_s_target_until_it_is_done() {
    let backend = Arc::new(Backend::default());
    let playback = playing(backend, snapshot());
    playback.report_position(Duration::from_secs(65)).unwrap();
    playback.seek_started(Duration::from_secs(300));
    let during = playback.position(Duration::from_secs(65));
    playback.seek_done();
    let after = playback.position(Duration::from_secs(296));
    assert_eq!(
        (during, after),
        (Duration::from_secs(300), Duration::from_secs(296))
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

const LENGTH: Option<Duration> = Some(Duration::from_secs(1800));

fn auto_skipped(actions: &[sdk::MarkerAction]) -> bool {
    actions
        .iter()
        .any(|a| matches!(a, sdk::MarkerAction::AutoSkip { .. }))
}

#[tokio::test]
async fn a_playback_s_markers_are_followed_from_its_info() {
    let playback = playing(Arc::new(Backend::default()), with_an_intro());
    let markers = playback.clone().follow_markers(false, true, true);
    assert_eq!(
        markers.on_position(Duration::from_secs(61), LENGTH),
        [sdk::MarkerAction::AutoSkip {
            to: Duration::from_secs(90)
        }]
    );
}

/// A seek the app told the playback (Android's engine does) holds the
/// markers too, so no second call is needed.
#[tokio::test]
async fn a_seek_told_the_playback_holds_the_markers() {
    let playback = playing(Arc::new(Backend::default()), with_an_intro());
    let markers = playback.clone().follow_markers(false, true, true);
    playback.seek_started(Duration::from_secs(300));
    assert_eq!(
        markers.position(Duration::from_secs(61)),
        Duration::from_secs(300)
    );
    assert!(!auto_skipped(
        &markers.on_position(Duration::from_secs(61), LENGTH)
    ));
}

/// The automatic skip is the playback's seek too: `report_position` saves
/// the target while the player still says the intro.
#[tokio::test]
async fn an_automatic_skip_holds_the_playback_s_position_at_its_target() {
    let playback = playing(Arc::new(Backend::default()), with_an_intro());
    let markers = playback.clone().follow_markers(false, true, true);
    markers.on_position(Duration::from_secs(61), LENGTH);
    assert_eq!(
        playback.position(Duration::from_secs(61)),
        Duration::from_secs(90)
    );
    assert_eq!(markers.undo_skip(), Some(Duration::from_secs(61)));
    assert_eq!(
        playback.position(Duration::from_secs(90)),
        Duration::from_secs(61)
    );
}

/// A re-plan moves the player to another token: the same playback, so
/// the same gate.
#[tokio::test(flavor = "multi_thread")]
async fn the_markers_keep_following_through_a_new_plan() {
    let playback = playing(Arc::new(Backend::default()), with_an_intro());
    let markers = playback.clone().follow_markers(false, true, true);
    plan_again_behind(&playback).await;
    playback.clone().follow_replan().await.unwrap().unwrap();
    playback.seek_started(Duration::from_secs(300));
    assert_eq!(
        markers.position(Duration::from_secs(61)),
        Duration::from_secs(300)
    );
}

/// A playback that is over seeks no more: no skip, no undo, no target.
#[tokio::test]
async fn a_finished_playback_has_no_skip_or_undo() {
    let playback = playing(Arc::new(Backend::default()), with_an_intro());
    let markers = playback.clone().follow_markers(false, true, true);
    assert!(auto_skipped(
        &markers.on_position(Duration::from_secs(61), LENGTH)
    ));
    playback.stop().await;
    assert!(!markers.undoable());
    assert_eq!(markers.undo_skip(), None);
    assert_eq!(markers.skip(), None);
}

#[tokio::test]
async fn a_finished_playback_skips_nothing_by_itself() {
    let playback = playing(Arc::new(Backend::default()), with_an_intro());
    let markers = playback.clone().follow_markers(false, true, true);
    playback.stop().await;
    assert!(!auto_skipped(
        &markers.on_position(Duration::from_secs(61), LENGTH)
    ));
}

/// Forgotten by the app, the object must not hold the playback open.
#[tokio::test]
async fn the_markers_do_not_keep_a_playback_open() {
    let playback = playing(Arc::new(Backend::default()), with_an_intro());
    let token = playback.token();
    let _markers = playback.clone().follow_markers(false, true, true);
    drop(playback);
    assert!(registered(&token).is_none());
}

/// `read` as a player's loading thread calls it.
async fn read_at(
    playback: &Arc<SpoolPlayback>,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, SpoolError> {
    let reading = playback.clone();
    tokio::task::spawn_blocking(move || reading.read(offset, length))
        .await
        .unwrap()
}

/// Wait for `seeking`, a condition a spawned seek makes true.
async fn until(seeking: impl Fn() -> bool) {
    while !seeking() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_seek_restarts_a_progressive_conversion_at_its_target() {
    let backend = Arc::new(Backend::default());
    let playback = playing(backend.clone(), converted(Delivery::Progressive));
    let moved = playback.clone().seek(Duration::from_secs(300)).await;
    assert_eq!(
        (
            moved.is_ok_and(|start| start.is_some()),
            backend.seeks.lock().unwrap().clone()
        ),
        (true, vec![Duration::from_secs(300)])
    );
}

/// The old encode's offsets mean nothing now: the next read is at 0, of
/// the new encode, whatever the reader had read.
#[tokio::test(flavor = "multi_thread")]
async fn the_read_after_a_seek_is_the_new_encode_from_its_start() {
    let playback = playing(Arc::default(), converted(Delivery::Progressive));
    let before = (
        read_at(&playback, 0, 1).await,
        read_at(&playback, 1, 1).await,
    );
    playback
        .clone()
        .seek(Duration::from_secs(300))
        .await
        .unwrap();
    assert_eq!(
        (before, read_at(&playback, 0, 1).await),
        ((Ok(vec![0]), Ok(vec![0])), Ok(vec![1]))
    );
}

/// A read waiting for the old encode is let go with the case every other
/// move of the player gives, for the same answer: `info()`, new media.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_waiting_through_a_seek_is_told_replanned() {
    let backend = Arc::new(Backend {
        output: Some(0),
        ..Default::default()
    });
    let playback = playing(backend, converted(Delivery::Progressive));
    let reading = playback.clone();
    let read = tokio::task::spawn_blocking(move || reading.read(0, 1));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let moved = playback.clone().seek(Duration::from_secs(300)).await;
    let read = tokio::time::timeout(Duration::from_secs(5), read).await;
    // A read still waiting on the old encode must not outlive the test.
    playback.set_flushing(true);
    assert_eq!(
        (moved.is_ok(), read.map(Result::unwrap)),
        (true, Ok(Err(SpoolError::Replanned)))
    );
}

/// The server has replaced the encode and the seek's answer is on its
/// way: a read that gets there first says the same, not
/// `PlaybackRestarted`, once the player has moved.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_ahead_of_the_seek_s_answer_is_told_replanned() {
    let backend = Arc::new(Backend::default());
    let playback = playing(backend.clone(), converted(Delivery::Progressive));
    let held = backend.seek_held.lock().await;
    let seeking = tokio::spawn(playback.clone().seek(Duration::from_secs(300)));
    until(|| playback.moving.try_lock().is_err()).await;
    backend
        .replaced
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let reading = playback.clone();
    let read = tokio::task::spawn_blocking(move || reading.read(0, 1));
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(held);
    let moved = seeking.await.unwrap();
    assert_eq!(
        (moved.is_ok_and(|s| s.is_some()), read.await.unwrap()),
        (true, Err(SpoolError::Replanned))
    );
}

/// What Android TV hit: the player starts over from byte 0 once it is
/// gone. It is told by name, and a seek gets it reading again.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_behind_the_reader_is_told_so_and_a_seek_reads_again() {
    let backend = Arc::new(Backend {
        output: Some(6 * MIB),
        ..Default::default()
    });
    let playback = playing(backend, converted(Delivery::Progressive));
    read_at(&playback, 3 * MIB + 2, 1).await.unwrap();
    let behind = read_at(&playback, 0, 1).await;
    playback
        .clone()
        .seek(Duration::from_secs(300))
        .await
        .unwrap();
    assert_eq!(
        (behind, read_at(&playback, 0, 1).await),
        (
            Err(SpoolError::ReadBehind {
                offset: 0,
                kept_from: 2 * MIB
            }),
            Ok(vec![1])
        )
    );
}

/// The app makes one call for every plan: `None` is its own player's seek
/// to make, and nothing goes to the server. The seek holds the position
/// all the same.
#[tokio::test]
async fn a_direct_or_hls_play_leaves_the_seek_to_its_player() {
    for playback in [snapshot(), converted(Delivery::Hls)] {
        let backend = Arc::new(Backend::default());
        let playback = playing(backend.clone(), playback);
        playback.report_position(Duration::from_secs(65)).unwrap();
        let moved = playback.clone().seek(Duration::from_secs(300)).await;
        assert_eq!(
            (
                moved.map(|start| start.is_none()),
                backend.seeks.lock().unwrap().clone(),
                playback.position(Duration::from_secs(65))
            ),
            (Ok(true), vec![], Duration::from_secs(300))
        );
    }
}

/// A held button or a scrubber: one restart, at the last target. The
/// seeks it replaced are `Cancelled`, for the app to drop.
#[tokio::test(flavor = "multi_thread")]
async fn seeks_in_a_row_gather_into_the_last() {
    let backend = Arc::new(Backend::default());
    let playback = playing(backend.clone(), converted(Delivery::Progressive));
    let first = tokio::spawn(playback.clone().seek(Duration::from_secs(10)));
    until(|| playback.seeks.load(Ordering::SeqCst) == 1).await;
    let last = playback.clone().seek(Duration::from_secs(20)).await;
    assert_eq!(
        (
            first.await.unwrap().map(|_| ()),
            last.is_ok_and(|s| s.is_some()),
            backend.seeks.lock().unwrap().clone()
        ),
        (
            Err(SpoolError::Cancelled),
            true,
            vec![Duration::from_secs(20)]
        )
    );
}

/// A seek the server refuses moves nothing: the player reads on, and its
/// positions count again.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_seek_leaves_the_player_where_it_was() {
    let backend = Arc::new(Backend {
        seek_refused: true.into(),
        ..Default::default()
    });
    let playback = playing(backend, converted(Delivery::Progressive));
    let before = playback.token();
    playback.report_position(Duration::from_secs(65)).unwrap();
    let moved = playback.clone().seek(Duration::from_secs(300)).await;
    assert_eq!(
        (
            moved.map(|_| ()),
            playback.token() == before,
            playback.position(Duration::from_secs(65)),
            read_at(&playback, 0, 1).await
        ),
        (
            Err(SpoolError::Busy),
            true,
            Duration::from_secs(65),
            Ok(vec![0])
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_finished_playback_seeks_no_more() {
    let backend = Arc::new(Backend::default());
    let playback = playing(backend.clone(), converted(Delivery::Progressive));
    playback.stop().await;
    let moved = playback.clone().seek(Duration::from_secs(300)).await;
    assert_eq!(moved.map(|_| ()), Err(SpoolError::PlaybackExpired));
}

/// A marker's target goes through `seek` like any other: the skip, then
/// its undo, each restart the encode, and the object still follows.
#[tokio::test(flavor = "multi_thread")]
async fn a_skip_and_its_undo_restart_a_conversion() {
    let backend = Arc::new(Backend::default());
    let intro = sdk::Playback {
        plan: converted(Delivery::Progressive).plan,
        ..with_an_intro()
    };
    let playback = playing(backend.clone(), intro);
    let markers = playback.clone().follow_markers(false, true, true);
    let skipped = markers.on_position(Duration::from_secs(61), LENGTH);
    let to = Duration::from_secs(90);
    assert_eq!(skipped, [sdk::MarkerAction::AutoSkip { to }]);
    playback.clone().seek(to).await.unwrap().unwrap();
    let during = markers.position(Duration::from_secs(61));
    let back = markers.undo_skip().unwrap();
    playback.clone().seek(back).await.unwrap().unwrap();
    assert_eq!(
        (
            during,
            back,
            markers.position(Duration::from_secs(90)),
            backend.seeks.lock().unwrap().clone()
        ),
        (to, Duration::from_secs(61), back, vec![to, back])
    );
}

/// The button pressed is no different.
#[tokio::test(flavor = "multi_thread")]
async fn a_pressed_skip_restarts_a_conversion() {
    let backend = Arc::new(Backend::default());
    let intro = sdk::Playback {
        plan: converted(Delivery::Progressive).plan,
        ..with_an_intro()
    };
    let playback = playing(backend.clone(), intro);
    let markers = playback.clone().follow_markers(false, false, true);
    markers.on_position(Duration::from_secs(61), LENGTH);
    let to = markers.skip().unwrap();
    playback.clone().seek(to).await.unwrap().unwrap();
    assert_eq!(backend.seeks.lock().unwrap().clone(), vec![to]);
}
